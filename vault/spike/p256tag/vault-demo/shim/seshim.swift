import CryptoKit
import Foundation
import LocalAuthentication
import Security
// C ABI for the KeyAgreement callback. No Mac-only API is used here.
private let lock = NSLock()
private var active: LAContext?
private func setActive(_ c: LAContext?) { lock.lock(); active = c; lock.unlock() }
private func code(_ e: Error) -> Int32 {
  let n = e as NSError
  fputs("enclave error: \(n.domain) \(n.code)\n", stderr)  // userInfo can hold biometry hashes
  let c = Int32(truncatingIfNeeded: n.code)
  return c == 0 ? 1 : c
}
@_cdecl("se_available") public func se_available() -> Int32 { SecureEnclave.isAvailable ? 1 : 0 }

@_cdecl("se_create")
public func se_create(
  _ biometry: Int32, _ blob: UnsafeMutablePointer<UInt8>, _ cap: Int,
  _ len: UnsafeMutablePointer<Int>, _ pub: UnsafeMutablePointer<UInt8>
) -> Int32 {
  do {
    var flags: SecAccessControlCreateFlags = [.privateKeyUsage]
    if biometry != 0 { flags.insert(.biometryCurrentSet) }
    var e: Unmanaged<CFError>?
    guard let ac = SecAccessControlCreateWithFlags(nil, kSecAttrAccessibleWhenUnlockedThisDeviceOnly, flags, &e)
    else { return -100 }
    let k = try SecureEnclave.P256.KeyAgreement.PrivateKey(accessControl: ac)
    let d = k.dataRepresentation
    if d.count > cap { return -101 }
    d.copyBytes(to: blob, count: d.count); len.pointee = d.count
    k.publicKey.x963Representation.copyBytes(to: pub, count: 65)
    return 0
  } catch { return code(error) }
}
/// Reading the public key needs no user presence: UI is forbidden here.
@_cdecl("se_public_key")
public func se_public_key(_ blob: UnsafePointer<UInt8>, _ n: Int, _ pub: UnsafeMutablePointer<UInt8>) -> Int32 {
  do {
    let c = LAContext(); c.interactionNotAllowed = true
    let k = try SecureEnclave.P256.KeyAgreement.PrivateKey(dataRepresentation: Data(bytes: blob, count: n), authenticationContext: c)
    k.publicKey.x963Representation.copyBytes(to: pub, count: 65)
    return 0
  } catch { return code(error) }
}
/// mode 0: UI forbidden. 1: may prompt. 2: UI forbidden and context already invalidated.
@_cdecl("se_ecdh")
public func se_ecdh(
  _ blob: UnsafePointer<UInt8>, _ n: Int, _ peer: UnsafePointer<UInt8>, _ mode: Int32,
  _ out: UnsafeMutablePointer<UInt8>
) -> Int32 {
  let ctx = LAContext()
  ctx.localizedReason = "unlock vault demo"
  if mode != 1 { ctx.interactionNotAllowed = true }
  if mode == 2 { ctx.invalidate() }
  setActive(ctx); defer { setActive(nil) }
  do {
    let k = try SecureEnclave.P256.KeyAgreement.PrivateKey(dataRepresentation: Data(bytes: blob, count: n), authenticationContext: ctx)
    let p = try P256.KeyAgreement.PublicKey(x963Representation: Data(bytes: peer, count: 65))
    let z = try k.sharedSecretFromKeyAgreement(with: p)
    z.withUnsafeBytes { out.update(from: $0.bindMemory(to: UInt8.self).baseAddress!, count: 32) }
    return 0
  } catch { return code(error) }
}
/// Dismisses a pending prompt; the in-flight se_ecdh then returns an error.
@_cdecl("se_cancel") public func se_cancel() { lock.lock(); active?.invalidate(); lock.unlock() }
