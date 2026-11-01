import CryptoKit
import Foundation
import LocalAuthentication
import Security

// The Secure Enclave behind a C ABI, for src/enclave.rs. It knows nothing of age.
// A key here is CryptoKit's dataRepresentation, held by the caller: nothing is
// stored on this machine, and there is no way to ask for a key without the gate.

private let lock = NSLock()
private var active: LAContext?

private func setActive(_ context: LAContext) {
  lock.lock()
  active = context
  lock.unlock()
}

/// Only its own: a prompt abandoned at the deadline must not clear the next one's.
private func clearActive(_ context: LAContext) {
  lock.lock()
  if active === context { active = nil }
  lock.unlock()
}

private func code(_ error: Error) -> Int32 {
  let e = error as NSError
  // The domain and code alone: userInfo can hold biometry hashes.
  fputs("enclave error: \(e.domain) \(e.code)\n", stderr)
  let c = Int32(truncatingIfNeeded: e.code)
  return c == 0 ? 1 : c
}

private func load(_ blob: UnsafePointer<UInt8>, _ length: Int, _ context: LAContext) throws
  -> SecureEnclave.P256.KeyAgreement.PrivateKey
{
  try SecureEnclave.P256.KeyAgreement.PrivateKey(
    dataRepresentation: Data(bytes: blob, count: length), authenticationContext: context)
}

private func forbiddingInterface() -> LAContext {
  let context = LAContext()
  context.interactionNotAllowed = true
  return context
}

@_cdecl("se_available")
public func se_available() -> Int32 { SecureEnclave.isAvailable ? 1 : 0 }

/// A new key that only the fingerprints enrolled now can use.
@_cdecl("se_create")
public func se_create(
  _ blob: UnsafeMutablePointer<UInt8>, _ capacity: Int, _ length: UnsafeMutablePointer<Int>
) -> Int32 {
  var failure: Unmanaged<CFError>?
  guard
    let access = SecAccessControlCreateWithFlags(
      nil, kSecAttrAccessibleWhenUnlockedThisDeviceOnly, [.privateKeyUsage, .biometryCurrentSet],
      &failure)
  else {
    failure?.release()
    return -100
  }
  do {
    let data = try SecureEnclave.P256.KeyAgreement.PrivateKey(accessControl: access)
      .dataRepresentation
    if data.count > capacity { return -101 }
    data.copyBytes(to: blob, count: data.count)
    length.pointee = data.count
    return 0
  } catch { return code(error) }
}

/// The interface is forbidden: this cannot prompt.
@_cdecl("se_public_key")
public func se_public_key(
  _ blob: UnsafePointer<UInt8>, _ length: Int, _ out: UnsafeMutablePointer<UInt8>
) -> Int32 {
  do {
    let key = try load(blob, length, forbiddingInterface())
    key.publicKey.x963Representation.copyBytes(to: out, count: 65)
    return 0
  } catch { return code(error) }
}

/// 1 if the key agrees with the interface forbidden, which a gated key cannot do,
/// and 0 for any refusal. Against a key made for this and dropped, so the secret
/// that comes of it means nothing. This cannot prompt.
@_cdecl("se_opens_unattended")
public func se_opens_unattended(_ blob: UnsafePointer<UInt8>, _ length: Int) -> Int32 {
  do {
    let key = try load(blob, length, forbiddingInterface())
    _ = try key.sharedSecretFromKeyAgreement(with: P256.KeyAgreement.PrivateKey().publicKey)
    return 1
  } catch { return 0 }
}

/// Fires Touch ID.
@_cdecl("se_ecdh")
public func se_ecdh(
  _ blob: UnsafePointer<UInt8>, _ length: Int, _ peer: UnsafePointer<UInt8>,
  _ out: UnsafeMutablePointer<UInt8>
) -> Int32 {
  let context = LAContext()
  context.localizedReason = "open a secret in the vault"
  setActive(context)
  defer { clearActive(context) }
  do {
    let key = try load(blob, length, context)
    let them = try P256.KeyAgreement.PublicKey(x963Representation: Data(bytes: peer, count: 65))
    let secret = try key.sharedSecretFromKeyAgreement(with: them)
    return secret.withUnsafeBytes { bytes -> Int32 in
      guard bytes.count == 32, let start = bytes.bindMemory(to: UInt8.self).baseAddress else {
        return -102
      }
      out.update(from: start, count: 32)
      return 0
    }
  } catch { return code(error) }
}

/// Dismisses a pending prompt; its se_ecdh then returns an error.
@_cdecl("se_cancel")
public func se_cancel() {
  lock.lock()
  active?.invalidate()
  lock.unlock()
}
