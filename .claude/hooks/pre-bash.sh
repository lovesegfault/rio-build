#!/usr/bin/env bash

# Pre-Bash hook: Warn about potentially dangerous commands
# Silent unless risky command detected

COMMAND="$1"

# Dangerous command patterns to warn about
DANGEROUS_PATTERNS=(
  'rm -rf /'
  'rm -rf ~'
  'rm -rf \*'
  'dd if='
  'mkfs\.'
  ':(){:|:&};:'  # Fork bomb
  'chmod -R 777'
  '> /dev/sd'
)

# Check for dangerous patterns
for pattern in "${DANGEROUS_PATTERNS[@]}"; do
  if echo "$COMMAND" | grep -qF "$pattern"; then
    echo "🚨 WARNING: Potentially destructive command detected!"
    echo "   Command: $COMMAND"
    echo "   This command could cause data loss or system damage"
    echo ""
    echo "   If you're certain this is correct, proceed with caution"
    # Don't block - just warn
    exit 0
  fi
done

# Warn about commands that modify system-wide settings
if echo "$COMMAND" | grep -qE '(^|\s)(sudo|doas)\s'; then
  if echo "$COMMAND" | grep -qE '(apt|yum|dnf|pacman|zypper) (remove|purge|erase)'; then
    echo "⚠️  Note: System package removal detected"
    echo "   Command: $COMMAND"
  fi
fi

exit 0
