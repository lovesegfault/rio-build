#!/usr/bin/env bash

# Pre-Edit hook: Scan for potential secrets before writing
# Silent unless issues found

FILE_PATH="$1"

# Skip checks for certain file types
case "$FILE_PATH" in
  *.md|*.txt|*.lock|*.sum|*.toml.example|*.env.example)
    exit 0
    ;;
esac

# Simple secret patterns (common API keys, tokens)
SECRET_PATTERNS=(
  'sk-[a-zA-Z0-9]{32,}'              # OpenAI-style API keys
  'ghp_[a-zA-Z0-9]{36,}'             # GitHub personal access tokens
  'gho_[a-zA-Z0-9]{36,}'             # GitHub OAuth tokens
  'ghs_[a-zA-Z0-9]{36,}'             # GitHub server tokens
  'AKIA[0-9A-Z]{16}'                 # AWS access key IDs
  '[a-zA-Z0-9+/]{40}'                # Generic base64-encoded secrets (40+ chars)
  'AIza[0-9A-Za-z_-]{35}'            # Google API keys
  'ya29\.[0-9A-Za-z_-]+'             # Google OAuth tokens
  '[0-9]+-[0-9A-Za-z_-]{32}\.apps\.googleusercontent\.com' # Google OAuth client IDs
  'xox[baprs]-[0-9]{10,13}-[0-9]{10,13}-[0-9A-Za-z]{24,}' # Slack tokens
)

# Read the new content from stdin
NEW_CONTENT=$(cat)

# Check for secret patterns
for pattern in "${SECRET_PATTERNS[@]}"; do
  if echo "$NEW_CONTENT" | grep -qE "$pattern"; then
    echo "⚠️  WARNING: Potential secret detected in edit to $FILE_PATH"
    echo "   Pattern matched: $pattern"
    echo "   Please review the content before proceeding"
    echo ""
    echo "   If this is intentional (e.g., test fixture), you can ignore this warning"
    # Don't block - just warn
    exit 0
  fi
done

exit 0
