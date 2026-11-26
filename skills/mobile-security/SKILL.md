---
name: mobile-security
description: "Mobile app security (Android/Kotlin/Java, iOS): exported components, deep links, WebView bridges, insecure storage, IPC. Load when the target is a mobile app or SDK (indexed languages include Kotlin and Java)."
---

# Mobile security

When the target is a mobile app or SDK:

## Android
- Exported activities/services/receivers without permission checks; `android:exported="true"` on components handling intents
- Deep links / app links with scheme+host not validated (intent redirection, open redirect to WebView)
- WebView with `addJavascriptInterface` (JS bridge to native), `setJavaScriptEnabled` on untrusted content, `setAllowFileAccess(true)`
- Sensitive data in SharedPreferences/logs/backup (`android:allowBackup="true"` with secrets), world-readable files
- Custom crypto, hardcoded keys in resources or native libs; certificate pinning absent or bypassable (checks in `onReceivedSslError` returning without failing)
- IPC via AIDL/Binder without signature-level permission checks

## iOS (when present in the tree)
- `application(_:open:)` opening URLs without validation; keychain items with `kSecAttrAccessibleAlways`

## When reviewing
- Reachability matters: an exported component with no permission is the finding; combine with what the intent extras control.
