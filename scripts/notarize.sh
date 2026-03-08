#!/usr/bin/env bash
set -euo pipefail

# Notarization script for Kaku macOS app
# Usage: ./scripts/notarize.sh [--staple-only]
#
# Prerequisites:
# 1. App must be signed with Developer ID
# 2. Set environment variables (or use macOS Keychain):
#    - KAKU_NOTARIZE_APPLE_ID: Your Apple ID email
#    - KAKU_NOTARIZE_TEAM_ID: Your Team ID (10 characters)
#    - KAKU_NOTARIZE_PASSWORD: App-specific password (not your Apple ID password)
#
# To generate app-specific password:
# https://appleid.apple.com/account/manage -> Sign-In and Security -> App-Specific Passwords

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

APP_NAME="Kaku"
OUT_DIR="${OUT_DIR:-dist}"
APP_BUNDLE="${OUT_DIR}/${APP_NAME}.app"
DMG_PATH="${OUT_DIR}/${APP_NAME}.dmg"

STAPLE_ONLY=0
for arg in "$@"; do
	case "$arg" in
	--staple-only) STAPLE_ONLY=1 ;;
	esac
done

# Check if app exists
if [[ ! -d "$APP_BUNDLE" ]]; then
	echo "Error: $APP_BUNDLE not found. Run ./scripts/build.sh first."
	exit 1
fi

# Verify signing
if ! codesign -v "$APP_BUNDLE" 2>/dev/null; then
	echo "Error: App is not signed. Run build with KAKU_SIGNING_IDENTITY set."
	exit 1
fi

echo "App: $APP_BUNDLE"
echo "DMG: $DMG_PATH"

# Get credentials from environment or Keychain
APPLE_ID="${KAKU_NOTARIZE_APPLE_ID:-}"
TEAM_ID="${KAKU_NOTARIZE_TEAM_ID:-}"
PASSWORD="${KAKU_NOTARIZE_PASSWORD:-}"

# If not set via env, try to read from Keychain
if [[ -z "$APPLE_ID" ]]; then
	echo "Checking Keychain for notarization credentials..."
	APPLE_ID=$(security find-generic-password -s "kaku-notarize-apple-id" -w 2>/dev/null || true)
fi

if [[ -z "$PASSWORD" ]]; then
	PASSWORD=$(security find-generic-password -s "kaku-notarize-password" -w 2>/dev/null || true)
fi

if [[ -z "$TEAM_ID" ]]; then
	# Try to extract from signing identity
	TEAM_ID=$(codesign -dv "$APP_BUNDLE" 2>&1 | grep TeamIdentifier | head -1 | awk -F= '{print $2}')
	if [[ -n "$TEAM_ID" ]]; then
		echo "Using Team ID from signature: $TEAM_ID"
	fi
fi

if [[ -z "$APPLE_ID" || -z "$PASSWORD" || -z "$TEAM_ID" ]]; then
	echo ""
	echo "Error: Notarization credentials not found."
	echo ""
	echo "Please set environment variables:"
	echo "  export KAKU_NOTARIZE_APPLE_ID='your-apple-id@example.com'"
	echo "  export KAKU_NOTARIZE_TEAM_ID='YOURTEAMID'"
	echo "  export KAKU_NOTARIZE_PASSWORD='xxxx-xxxx-xxxx-xxxx'"
	echo ""
	echo "Or store in Keychain:"
	echo "  security add-generic-password -s 'kaku-notarize-apple-id' -a 'kaku' -w 'your-apple-id@example.com'"
	echo "  security add-generic-password -s 'kaku-notarize-password' -a 'kaku' -w 'your-app-specific-password'"
	echo ""
	echo "To generate app-specific password: https://appleid.apple.com/account/manage"
	exit 1
fi

if [[ "$STAPLE_ONLY" == "1" ]]; then
	echo "Stapling existing notarization ticket..."

	echo "Stapling app bundle..."
	xcrun stapler staple "$APP_BUNDLE"

	if [[ -f "$DMG_PATH" ]]; then
		echo "Stapling DMG..."
		xcrun stapler staple "$DMG_PATH"
	fi

	echo "✅ Staple complete!"
	echo ""
	echo "Verifying notarization:"
	spctl -a -vv "$APP_BUNDLE" 2>&1 || true
	exit 0
fi

# Submit for notarization
echo "Submitting for notarization..."
echo "  Apple ID: $APPLE_ID"
echo "  Team ID: $TEAM_ID"

# Submit the DMG if it exists, otherwise submit the app
if [[ -f "$DMG_PATH" ]]; then
	SUBMISSION_PATH="$DMG_PATH"
	echo "  Submitting DMG..."
else
	SUBMISSION_PATH="$APP_BUNDLE"
	echo "  Submitting app bundle..."
fi

# Submit and capture output
echo ""
echo "Uploading to Apple notarization service (this may take a few minutes)..."
SUBMIT_OUTPUT=$(xcrun notarytool submit "$SUBMISSION_PATH" \
	--apple-id "$APPLE_ID" \
	--team-id "$TEAM_ID" \
	--password "$PASSWORD" \
	--wait 2>&1) || {
	echo "Notarization submission failed:"
	echo "$SUBMIT_OUTPUT"
	exit 1
}

echo "$SUBMIT_OUTPUT"

# Check if accepted
if echo "$SUBMIT_OUTPUT" | grep -q "Accepted"; then
	echo ""
	echo "✅ Notarization accepted! Stapling ticket..."

	xcrun stapler staple "$APP_BUNDLE"

	if [[ -f "$DMG_PATH" ]]; then
		xcrun stapler staple "$DMG_PATH"
	fi

	echo ""
	echo "✅ Done! App is notarized and ready for distribution."
	echo ""
	echo "Verifying notarization:"
	spctl -a -vv "$APP_BUNDLE" 2>&1 || true
else
	echo ""
	echo "❌ Notarization failed or returned unexpected status."
	echo "Full output:"
	echo "$SUBMIT_OUTPUT"

	# Extract submission ID and fetch detailed log
	SUBMISSION_ID=$(echo "$SUBMIT_OUTPUT" | grep "id:" | head -1 | awk '{print $2}')
	if [[ -n "$SUBMISSION_ID" ]]; then
		echo ""
		echo "Fetching detailed notarization log..."
		xcrun notarytool log "$SUBMISSION_ID" \
			--apple-id "$APPLE_ID" \
			--team-id "$TEAM_ID" \
			--password "$PASSWORD" 2>&1 || true
	fi

	exit 1
fi
