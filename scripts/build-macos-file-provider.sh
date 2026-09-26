#!/bin/bash
# Builds isolated experimental artifacts. Does not alter the running app,
# register domains, install profiles, or weaken macOS extension validation.
set -euo pipefail
cd "$(dirname "$0")/.."
local_probe=false
case "${1:-}" in
  "") ;;
  --local-sign) local_probe=true ;;
  *) echo "Usage: $0 [--local-sign]" >&2; exit 2 ;;
esac
arch_name="$(uname -m)"
case "$arch_name" in arm64|x86_64) ;; *) echo 'macOS host required' >&2; exit 1 ;; esac
output="$PWD/target/debug/file-provider"
host="$output/CloudreveFinderPreview.app"
extension="$host/Contents/PlugIns/CloudreveFileProvider.appex"
mkdir -p "$extension/Contents/MacOS" "$host/Contents/MacOS" "$output/module-cache"
MACOSX_DEPLOYMENT_TARGET=12.3 cargo build --locked --offline -p cloudreve-file-provider
swift_flags=(-swift-version 5 -target "$arch_name-apple-macos12.3" -module-cache-path "$output/module-cache" -import-objc-header macos/FileProvider/Bridge.h)
link_flags=(-L target/debug -lcloudreve_file_provider -framework FileProvider -framework Foundation -framework UniformTypeIdentifiers -framework Security -framework SystemConfiguration -framework CoreFoundation -liconv -lc++ -lresolv)
xcrun swiftc "${swift_flags[@]}" macos/FileProvider/{Configuration,Item}.swift macos/Tests/ItemTests.swift -o "$output/item-tests"
"$output/item-tests"
xcrun swiftc "${swift_flags[@]}" -parse-as-library macos/FileProvider/*.swift "${link_flags[@]}" -lextension -Xlinker -e -Xlinker _NSExtensionMain -o "$extension/Contents/MacOS/CloudreveFileProvider"
xcrun swiftc "${swift_flags[@]}" -parse-as-library macos/FileProvider/{Configuration,Backend,Item}.swift macos/Host/main.swift "${link_flags[@]}" -o "$host/Contents/MacOS/cloudreve-provider-host"
python3 - "$extension/Contents/Info.plist" "$host/Contents/Info.plist" <<'PY'
import plistlib,sys
with open(sys.argv[1],'wb') as f:
 plistlib.dump({
  'CFBundleIdentifier':'cloudreve.desktop.finder-preview.fileprovider',
  'CFBundleName':'Cloudreve File Provider',
  'CFBundleExecutable':'CloudreveFileProvider',
  'CFBundlePackageType':'XPC!',
  'CFBundleVersion':'1', 'CFBundleShortVersionString':'0.2.0',
  'LSMinimumSystemVersion':'12.3',
  'CloudreveKeychainGroup':'$(AppIdentifierPrefix)cloudreve.desktop.finder-preview',
  'NSExtension':{
   'NSExtensionPointIdentifier':'com.apple.fileprovider-nonui',
   'NSExtensionPrincipalClass':'CloudreveFileProvider',
   'NSExtensionFileProviderSupportsEnumeration':True,
  },
 },f)
with open(sys.argv[2],'wb') as f:
 plistlib.dump({
  'CFBundleIdentifier':'cloudreve.desktop.finder-preview',
  'CFBundleName':'Cloudreve Finder Preview',
  'CFBundleExecutable':'cloudreve-provider-host',
  'CFBundlePackageType':'APPL',
  'CFBundleVersion':'1', 'CFBundleShortVersionString':'0.2.0',
  'LSMinimumSystemVersion':'12.3', 'LSUIElement':True,
  'CloudreveKeychainGroup':'$(AppIdentifierPrefix)cloudreve.desktop.finder-preview',
 },f)
PY
if "$local_probe"; then
  # Local ad-hoc development build: sandboxed extension and login Keychain
  # ACL restricted to the host and extension executables. No Apple identity.
  python3 - "$output/local-probe-entitlements.plist" "$host/Contents/Info.plist" "$extension/Contents/Info.plist" <<'PYLOCAL'
import plistlib,sys
with open(sys.argv[1], 'wb') as f:
 plistlib.dump({'com.apple.security.app-sandbox':True, 'com.apple.security.network.client':True}, f)
for path in sys.argv[2:]:
 with open(path, 'rb') as f: info=plistlib.load(f)
 info['CloudreveLocalRegistrationProbe']=True
 info['CloudreveLocalKeychain']=True
 with open(path, 'wb') as f: plistlib.dump(info,f)
PYLOCAL
  codesign --force --sign - --entitlements "$output/local-probe-entitlements.plist" "$extension"
  codesign --force --sign - "$host"
  codesign --verify --strict --verbose=2 "$extension"
  codesign --verify --strict --verbose=2 "$host"
fi
printf 'Experimental artifacts (local signing: %s): %s\n' "$local_probe" "$output"
