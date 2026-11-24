package xyz.justzappit.atomicswap

import java.math.BigInteger

object AtomicSwap {
    fun userShare(key: SwapKey): ByteArray = AtomicSwapNative.userShare(key.seed, key.mainnet, key.index)

    fun authAddress(key: SwapKey): ByteArray = AtomicSwapNative.authAddress(key.seed, key.mainnet, key.index)

    fun claimSecret(key: SwapKey): ByteArray = AtomicSwapNative.claimSecret(key.seed, key.mainnet, key.index)

    fun payoutNote(
        key: SwapKey,
        railgun: RailgunSeed,
    ): PayoutNote {
        val words = AtomicSwapNative.payoutNote(key.seed, key.mainnet, key.index, railgun.bytes).asWords()
        return PayoutNote(npk = words[0], encryptedBundle = words.subList(1, 4), shieldKey = words[4], commitment = words[5])
    }

    fun shieldNote(
        seed: ByteArray,
        mainnet: Boolean,
        account: Int,
        index: Int,
        validBefore: Long,
        railgun: RailgunSeed,
    ): DepositShieldNote = depositNote(seed, mainnet, account, index, railgun, rescue = false, number = validBefore)

    fun rescueNote(
        seed: ByteArray,
        mainnet: Boolean,
        account: Int,
        index: Int,
        nonce: Long,
        railgun: RailgunSeed,
    ): DepositShieldNote = depositNote(seed, mainnet, account, index, railgun, rescue = true, number = nonce)

    fun accept(
        key: SwapKey,
        railgun: RailgunSeed,
        deployment: Deployment,
        quoteId: ByteArray,
        makerShare: ByteArray,
        makerProof: ByteArray,
    ): Acceptance {
        val bytes =
            AtomicSwapNative.accept(
                key.seed,
                key.mainnet,
                key.index,
                railgun.bytes,
                deployment.chainId,
                deployment.contract,
                quoteId,
                makerShare,
                makerProof,
            )
        return Acceptance(
            userShare = bytes.copyOfRange(0, SHARE_BYTES),
            userProof = bytes.copyOfRange(SHARE_BYTES, 2 * SHARE_BYTES),
            viewingKeys = bytes.copyOfRange(2 * SHARE_BYTES, 3 * SHARE_BYTES),
        )
    }

    fun checkOpenFee(
        mainnet: Boolean,
        fee: OpenFee,
        maxZat: Long,
    ) = AtomicSwapNative.checkOpenFee(mainnet, fee.address, fee.zat, maxZat)

    fun depositAccount(
        key: SwapKey,
        makerShare: ByteArray,
    ): DepositAccount {
        val (address, ufvk) = AtomicSwapNative.depositAccount(key.seed, key.mainnet, key.index, makerShare)
        return DepositAccount(address, ufvk)
    }

    fun signLockClaim(
        key: SwapKey,
        deployment: Deployment,
        swapId: ByteArray,
        deadline: Long,
    ): ByteArray =
        AtomicSwapNative.signLockClaim(
            key.seed,
            key.mainnet,
            key.index,
            deployment.chainId,
            deployment.contract,
            swapId,
            deadline,
        )

    fun signPayout(
        key: SwapKey,
        deployment: Deployment,
        swapId: ByteArray,
        relayer: ByteArray,
        fee: BigInteger,
    ): ByteArray =
        AtomicSwapNative.signPayout(
            key.seed,
            key.mainnet,
            key.index,
            deployment.chainId,
            deployment.contract,
            swapId,
            relayer,
            fee.toString(),
        )

    fun signRefund(
        key: SwapKey,
        makerShare: ByteArray,
        makerSecret: ByteArray,
        pczt: ByteArray,
        intent: SweepIntent,
    ): ByteArray = AtomicSwapNative.signRefund(
        key.seed, key.mainnet, key.index, makerShare, makerSecret, pczt,
        intent.recipient, intent.minimumReceivedZat, intent.maximumFeeZat,
    )

    fun railgunAddress(railgun: RailgunSeed): String = AtomicSwapNative.railgunAddress(railgun.bytes)

    private fun depositNote(
        seed: ByteArray,
        mainnet: Boolean,
        account: Int,
        index: Int,
        railgun: RailgunSeed,
        rescue: Boolean,
        number: Long,
    ): DepositShieldNote {
        val words = AtomicSwapNative.depositNote(seed, mainnet, account, index, railgun.bytes, rescue, number).asWords()
        return DepositShieldNote(
            npk = words[0],
            encryptedBundle = words.subList(1, 4),
            shieldKey = words[4],
            commitment = words[5],
        )
    }

    private const val SHARE_BYTES = 64
    private const val WORD_BYTES = 32

    private fun ByteArray.asWords() = (indices step WORD_BYTES).map { copyOfRange(it, it + WORD_BYTES) }
}

data class SweepIntent(
    val recipient: String,
    val minimumReceivedZat: Long,
    val maximumFeeZat: Long,
) {
    init {
        require(minimumReceivedZat > 0 && maximumFeeZat >= 0)
    }
}

class SwapKey(
    val seed: ByteArray,
    val mainnet: Boolean,
    val index: Int,
)

class RailgunSeed(
    val bytes: ByteArray,
)

class Deployment(
    val chainId: Long,
    val contract: ByteArray,
)

class PayoutNote(
    val npk: ByteArray,
    val encryptedBundle: List<ByteArray>,
    val shieldKey: ByteArray,
    val commitment: ByteArray,
)

class DepositShieldNote(
    val npk: ByteArray,
    val encryptedBundle: List<ByteArray>,
    val shieldKey: ByteArray,
    val commitment: ByteArray,
)

class Acceptance(
    val userShare: ByteArray,
    val userProof: ByteArray,
    val viewingKeys: ByteArray,
)

data class OpenFee(
    val zat: Long,
    val address: String,
)

data class DepositAccount(
    val address: String,
    val ufvk: String,
)

class AtomicSwapException(
    message: String,
) : Exception(message)
