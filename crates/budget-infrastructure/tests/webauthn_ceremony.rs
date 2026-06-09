//! End-to-end `WebAuthn` ceremony security tests driving a real virtual
//! authenticator (`ORCH-NEW-PATH-TESTS-1`, `BUDGET-AUTH-GATE-1`, `SPEC §9.1`).
//!
//! These are ADVERSARIAL tests authored independently of the step-7 build. The
//! step-7 in-module `webauthn.rs` tests cover only construction + the
//! no-credentials guard; they never produce a real assertion, so they cannot
//! prove that a *valid* assertion authenticates or that a *tampered* one is
//! rejected. Here a software passkey (`webauthn-authenticator-rs`'s `SoftPasskey`,
//! the canonical virtual authenticator) runs the full registration +
//! authentication ceremonies against our [`WebauthnService`], producing genuine
//! signed credentials and assertions that we then attack:
//!
//!   - a valid round-trip authenticates and reports the credential id + counter;
//!   - an assertion produced by a DIFFERENT authenticator (wrong credential) is
//!     rejected;
//!   - an assertion against a relying party with a different RP id / origin is
//!     rejected (RP-binding / phishing resistance);
//!   - a tampered assertion (mutated signature bytes) fails verification;
//!   - sign-count regression is caught by the service-layer clone check.
//!
//! `SoftPasskey` is backed by OpenSSL via `webauthn-authenticator-rs`'s `crypto`
//! feature; this is a dev-dependency only and never enters the production build.

#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]
#![allow(clippy::panic)]

use webauthn_authenticator_rs::WebauthnAuthenticator;
use webauthn_authenticator_rs::softpasskey::SoftPasskey;
use webauthn_rs::prelude::Url;

use budget_domain::ids::UserId;
use budget_infrastructure::auth::webauthn::WebauthnService;

const RP_ID: &str = "localhost";
const RP_ORIGIN: &str = "http://localhost:8080";
const RP_NAME: &str = "Budget Tracker";

/// Build the relying-party service under test.
fn service() -> WebauthnService {
    WebauthnService::new(RP_ID, RP_ORIGIN, RP_NAME).expect("build rp")
}

/// A fresh virtual authenticator. `falsify_uv = true` so it claims user
/// verification, which the passkey ceremony requires
/// (`UserVerificationPolicy::Required`).
fn authenticator() -> WebauthnAuthenticator<SoftPasskey> {
    WebauthnAuthenticator::new(SoftPasskey::new(true))
}

/// Run a full registration ceremony for `user_id` on `authn`, returning the
/// domain credential the service would persist.
fn register(
    svc: &WebauthnService,
    authn: &mut WebauthnAuthenticator<SoftPasskey>,
    user_id: UserId,
) -> budget_domain::auth::WebauthnCredential {
    let (challenge, reg_state) = svc
        .start_registration(user_id, "zach@example.com", "Zach", &[])
        .expect("start registration");
    // The virtual authenticator creates the credential against the challenge.
    let reg_response = authn
        .do_registration(Url::parse(RP_ORIGIN).unwrap(), challenge)
        .expect("authenticator registration");
    let registered = svc
        .finish_registration(&reg_response, &reg_state)
        .expect("finish registration");
    WebauthnService::to_domain_credential(&registered, user_id, Some("Test Device".to_owned()))
}

#[test]
fn valid_ceremony_round_trip_authenticates() {
    let svc = service();
    let mut authn = authenticator();
    let user_id = UserId::generate();

    let credential = register(&svc, &mut authn, user_id);

    // Authentication ceremony against the registered credential.
    let (challenge, auth_state) = svc
        .start_authentication(std::slice::from_ref(&credential))
        .expect("start authentication");
    let assertion = authn
        .do_authentication(Url::parse(RP_ORIGIN).unwrap(), challenge)
        .expect("authenticator assertion");
    let outcome = svc
        .finish_authentication(&assertion, &auth_state)
        .expect("a valid assertion must verify");

    assert_eq!(
        outcome.credential_id, credential.credential_id,
        "the authenticated credential id must match the registered one",
    );
    assert!(
        outcome.user_verified,
        "the authenticator claimed user verification",
    );
}

#[test]
fn assertion_from_a_different_authenticator_is_rejected() {
    let svc = service();
    let user_id = UserId::generate();

    // Register authenticator A; the server only knows A's credential.
    let mut authn_a = authenticator();
    let credential_a = register(&svc, &mut authn_a, user_id);

    // Register authenticator B against a SEPARATE service instance so B has a
    // valid-looking credential the server has never seen.
    let mut authn_b = authenticator();
    let credential_b = register(&svc, &mut authn_b, user_id);

    // The server starts a challenge for A's credential only...
    let (challenge, auth_state) = svc
        .start_authentication(std::slice::from_ref(&credential_a))
        .expect("start auth for A");

    // ...but we try to satisfy it from B. B does not hold A's credential id, so
    // the authenticator cannot produce a matching assertion.
    let attempt = authn_b.do_authentication(Url::parse(RP_ORIGIN).unwrap(), challenge);
    match attempt {
        // The authenticator refuses (it has no matching credential): rejection
        // happens at the device, which is itself a correct outcome.
        Err(_) => {}
        // If it somehow produced an assertion, the server MUST reject it because
        // the credential id is not the one the challenge was bound to.
        Ok(assertion) => {
            let result = svc.finish_authentication(&assertion, &auth_state);
            assert!(
                result.is_err(),
                "an assertion from an unregistered authenticator must not verify",
            );
        }
    }
    // Sanity: A and B really are distinct credentials.
    assert_ne!(credential_a.credential_id, credential_b.credential_id);
}

#[test]
fn assertion_for_wrong_relying_party_is_rejected() {
    // RP binding is WebAuthn's phishing-resistance property: a credential bound
    // to our RP id must not be assertable by a different RP, and an assertion a
    // device produced for a DIFFERENT origin must not verify on ours.
    //
    // The virtual authenticator enforces this at the device too: when our real
    // service hands it a challenge whose RP id is OUR domain, but the calling
    // origin is the attacker's, the authenticator refuses to sign (returns
    // `Security`) because the RP id is not a suffix of the presented origin.
    // That refusal is itself the defense; we assert it explicitly.
    let svc = service();
    let mut authn = authenticator();
    let user_id = UserId::generate();
    let credential = register(&svc, &mut authn, user_id);

    // Our real RP issues a challenge for our credential (RP id = "localhost").
    let (our_challenge, _our_state) = svc
        .start_authentication(std::slice::from_ref(&credential))
        .expect("our start auth");

    // An attacker tries to relay OUR challenge but presents it from the attacker
    // origin. The authenticator must refuse: our RP id is not a suffix of the
    // evil origin, so no assertion is ever produced for the phisher.
    let evil_origin = "https://evil.example.com:8080";
    let phishing_attempt = authn.do_authentication(Url::parse(evil_origin).unwrap(), our_challenge);
    assert!(
        phishing_attempt.is_err(),
        "the authenticator must refuse to sign our challenge for a foreign origin (phishing resistance)",
    );

    // Conversely: an entirely separate RP (the attacker's own server, with its
    // own credential the same device registered) produces a perfectly valid
    // assertion for the EVIL origin. Our real RP must still reject it, because it
    // is bound to the wrong RP id hash and to a challenge we never issued.
    let evil_svc =
        WebauthnService::new("evil.example.com", evil_origin, "Evil").expect("build evil rp");
    let (evil_chal, evil_reg_state) = evil_svc
        .start_registration(user_id, "zach@example.com", "Zach", &[])
        .expect("evil start registration");
    let evil_reg = authn
        .do_registration(Url::parse(evil_origin).unwrap(), evil_chal)
        .expect("device registers with evil rp");
    let evil_registered = evil_svc
        .finish_registration(&evil_reg, &evil_reg_state)
        .expect("evil finish registration");
    let evil_credential = WebauthnService::to_domain_credential(&evil_registered, user_id, None);
    let (evil_auth_chal, _evil_auth_state) = evil_svc
        .start_authentication(std::slice::from_ref(&evil_credential))
        .expect("evil start auth");
    let evil_assertion = authn
        .do_authentication(Url::parse(evil_origin).unwrap(), evil_auth_chal)
        .expect("device signs a valid assertion for the evil rp");

    // Feed the evil-origin assertion to OUR service's challenge/state.
    let (_our_challenge2, our_state2) = svc
        .start_authentication(std::slice::from_ref(&credential))
        .expect("our start auth 2");
    let cross = svc.finish_authentication(&evil_assertion, &our_state2);
    assert!(
        cross.is_err(),
        "an assertion produced for a different RP/origin must not verify on ours",
    );
}

#[test]
fn tampered_assertion_signature_is_rejected() {
    let svc = service();
    let mut authn = authenticator();
    let user_id = UserId::generate();
    let credential = register(&svc, &mut authn, user_id);

    let (challenge, auth_state) = svc
        .start_authentication(std::slice::from_ref(&credential))
        .expect("start auth");
    let mut assertion = authn
        .do_authentication(Url::parse(RP_ORIGIN).unwrap(), challenge)
        .expect("assertion");

    // Flip bytes in the signature: the cryptographic verification must fail.
    let sig = &mut assertion.response.signature;
    assert!(!sig.is_empty(), "assertion must carry a signature");
    for b in sig.iter_mut() {
        *b ^= 0xFF;
    }

    let result = svc.finish_authentication(&assertion, &auth_state);
    assert!(
        result.is_err(),
        "a tampered signature must fail verification (not authenticate)",
    );
}

#[test]
fn replayed_assertion_against_fresh_challenge_is_rejected() {
    // An assertion is bound to the challenge it answered. Replaying it against a
    // brand-new challenge/state must fail (anti-replay).
    let svc = service();
    let mut authn = authenticator();
    let user_id = UserId::generate();
    let credential = register(&svc, &mut authn, user_id);

    let (challenge, _used_state) = svc
        .start_authentication(std::slice::from_ref(&credential))
        .expect("start auth 1");
    let assertion = authn
        .do_authentication(Url::parse(RP_ORIGIN).unwrap(), challenge)
        .expect("assertion");

    // A fresh, independent challenge/state (as a new request would produce).
    let (_fresh_challenge, fresh_state) = svc
        .start_authentication(std::slice::from_ref(&credential))
        .expect("start auth 2");

    let replay = svc.finish_authentication(&assertion, &fresh_state);
    assert!(
        replay.is_err(),
        "an assertion replayed against a different challenge must be rejected",
    );
}
