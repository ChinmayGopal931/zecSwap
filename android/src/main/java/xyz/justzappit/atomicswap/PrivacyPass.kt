package xyz.justzappit.atomicswap

object PrivacyPass {
    fun readChallenge(header: String): PrivateTokenChallenge {
        val (challenge, tokenKey, issuer) = AtomicSwapNative.readTokenChallenge(header)
        return PrivateTokenChallenge(challenge, issuer.decodeToString(), tokenKey)
    }

    fun blind(
        challenge: ByteArray,
        tokenKey: ByteArray,
    ): PrivateTokenRequest {
        val bytes = AtomicSwapNative.blindToken(tokenKey, challenge)
        val request =
            PrivateTokenRequest(
                blinded = bytes.copyOfRange(0, BLINDED_BYTES),
                pending = PendingToken(bytes.copyOfRange(BLINDED_BYTES, bytes.size)),
            )
        bytes.fill(0)
        return request
    }

    fun finalize(
        pending: PendingToken,
        tokenKey: ByteArray,
        blindSignature: ByteArray,
    ): ByteArray = AtomicSwapNative.finalizeToken(pending.bytes, tokenKey, blindSignature)

    fun authorization(token: ByteArray): String = AtomicSwapNative.tokenAuthorization(token)

    private const val BLINDED_BYTES = 256
}

class PrivateTokenChallenge(
    val challenge: ByteArray,
    val issuer: String,
    val tokenKey: ByteArray,
)

class PrivateTokenRequest(
    val blinded: ByteArray,
    val pending: PendingToken,
)

class PendingToken(
    val bytes: ByteArray,
) {
    override fun toString() = "PendingToken"
}
