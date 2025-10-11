#!/usr/bin/env bash

# Post-Edit hook: Auto-format Rust files after editing
# Only formats the specific file that was edited

FILE_PATH="$1"

# Only process Rust files
if [[ "$FILE_PATH" != *.rs ]]; then
  exit 0
fi

# Check if file exists
if [ ! -f "$FILE_PATH" ]; then
  exit 0
fi

# Check if rustfmt is available
if ! command -v rustfmt &> /dev/null; then
  # Silent - rustfmt not available in this environment
  exit 0
fi

# Format the file silently
if rustfmt --edition 2024 "$FILE_PATH" 2>/dev/null; then
  # Success - no output needed
  exit 0
else
  # Only report errors
  echo "⚠️  rustfmt failed on $FILE_PATH"
  echo "   Run manually: rustfmt --edition 2024 $FILE_PATH"
  exit 0
fi
