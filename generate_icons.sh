#!/bin/bash
set -e

# Ensure assets directory exists
mkdir -p assets/icon.iconset

INPUT_SVG="assets/icon.svg"

if ! command -v rsvg-convert &> /dev/null; then
    echo "Error: rsvg-convert could not be found. Please install librsvg2-bin or equivalent."
    exit 1
fi

if ! command -v convert &> /dev/null; then
    echo "Warning: convert (ImageMagick) not found. ICO file will not be generated."
fi

echo "Rendering icons..."

# Function to render icon
render_icon() {
    SIZE=$1
    NAME=$2
    rsvg-convert -w $SIZE -h $SIZE "$INPUT_SVG" -o "assets/icon.iconset/$NAME"
}

# Standard sizes for macOS iconset
# 16x16
render_icon 16 "icon_16x16.png"
render_icon 32 "icon_16x16@2x.png"
# 32x32
render_icon 32 "icon_32x32.png"
render_icon 64 "icon_32x32@2x.png"
# 128x128
render_icon 128 "icon_128x128.png"
render_icon 256 "icon_128x128@2x.png"
# 256x256
render_icon 256 "icon_256x256.png"
render_icon 512 "icon_256x256@2x.png"
# 512x512
render_icon 512 "icon_512x512.png"
render_icon 1024 "icon_512x512@2x.png"

echo "Mac OS .iconset created."

# Linux Icon (just a high res png)
cp assets/icon.iconset/icon_512x512.png assets/icon.png
echo "Linux icon.png created."

# Windows ICO
if command -v convert &> /dev/null; then
    # Create 48x48 specifically for Windows if not already overlapping
    render_icon 48 "icon_48x48.png"
    
    convert assets/icon.iconset/icon_16x16.png \
            assets/icon.iconset/icon_32x32.png \
            assets/icon.iconset/icon_48x48.png \
            assets/icon.iconset/icon_128x128.png \
            assets/icon.iconset/icon_256x256.png \
            assets/icon.ico
    
    # Clean up the windows-specific 48px if it's strictly mostly for ICO
    rm assets/icon.iconset/icon_48x48.png
    
    echo "Windows icon.ico created."
else
    echo "Skipping .ico generation (convert missing)"
fi

echo "Done! Icons are in the 'assets' folder."
