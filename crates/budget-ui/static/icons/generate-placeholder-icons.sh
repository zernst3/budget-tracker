#!/bin/bash
# Placeholder icon generator for Budget Tracker PWA.
# This script creates simple placeholder icons. Zach can replace these
# with real branded icons.

# Create a simple SVG icon (192x192 base)
cat > icon-base.svg << 'SVG'
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 192 192">
  <!-- Background circle -->
  <rect width="192" height="192" fill="#1f2937"/>
  <!-- Dollar sign as a simple line-based symbol -->
  <g transform="translate(96, 96)" stroke="#ffffff" stroke-width="12" fill="none" stroke-linecap="round" stroke-linejoin="round">
    <!-- Vertical line -->
    <line x1="0" y1="-40" x2="0" y2="40"/>
    <!-- Top curve (upper S) -->
    <path d="M -30 -20 Q -30 -30 0 -30 Q 30 -30 30 -20"/>
    <!-- Bottom curve (lower S) -->
    <path d="M -30 20 Q -30 30 0 30 Q 30 30 30 20"/>
  </g>
</svg>
SVG

# Use imagemagick if available to convert to PNG (optional)
# For now, document that this is a placeholder
echo "Placeholder icon SVG created: icon-base.svg"
echo "TODO: Replace these with real branded icons using your design tool."
echo "Required sizes:"
echo "  - icon-192x192.png (192x192)"
echo "  - icon-512x512.png (512x512)"
echo "  - maskable-icon-192x192.png (192x192, safe zone ≤80x80)"
echo "  - maskable-icon-512x512.png (512x512, safe zone ≤204x204)"
echo "  - screenshot-540x720.png (540x720, app screenshot)"
