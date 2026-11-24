const path = require('path');
const msgpack = require('msgpack-lite');

const dist = path.dirname(require.resolve('@railgun-community/engine'));
const load = (module) => require(path.join(dist, module));

const { deriveNodes, WalletNode } = load('key-derivation/wallet-node');
const { getMasterKeyFromSeed } = load('key-derivation/bip32');
const { encodeAddress } = load('key-derivation/bech32');
const { Babyjubjub } = load('key-derivation/babyjubjub');
const { getSharedSymmetricKey, getPublicViewingKey, getNoteBlindingKeys } = load('utils/keys-utils');
const { initPoseidonPromise } = load('utils/poseidon');
const { initCurve25519Promise } = load('utils/scalar-multiply');
const { MEMO_SENDER_RANDOM_NULL } = load('models/transaction-constants');
const WalletInfo = load('wallet/wallet-info').default;
const {
  ShieldNote,
  ShieldNoteERC20,
  Mnemonic,
  TransactNote,
  OutputType,
  TXIDVersion,
  getTokenDataERC20,
} = require('@railgun-community/engine');

const hex = (bytes) => `0x${Buffer.from(bytes).toString('hex')}`;
const bytes = (value) => Buffer.from(value.replace(/^0x/, ''), 'hex');
const word = (n) => `0x${n.toString(16).padStart(64, '0')}`;

async function wallet(seed, index) {
  await Promise.all([initPoseidonPromise, initCurve25519Promise]);
  const master = new WalletNode(getMasterKeyFromSeed(seed.replace(/^0x/, '')));
  const nodes = {
    spending: master.derive(`m/44'/1984'/0'/0'/${index}'`),
    viewing: master.derive(`m/420'/1984'/0'/0'/${index}'`),
  };
  const spending = nodes.spending.getSpendingKeyPair();
  const viewing = await nodes.viewing.getViewingKeyPair();
  const nullifyingKey = await nodes.viewing.getNullifyingKey();
  const masterPublicKey = WalletNode.getMasterPublicKey(spending.pubkey, nullifyingKey);
  return {
    seed,
    index,
    spendingPrivateKey: hex(spending.privateKey),
    spendingPublicKey: spending.pubkey.map(word),
    viewingPrivateKey: hex(viewing.privateKey),
    viewingPublicKey: hex(viewing.pubkey),
    nullifyingKey: word(nullifyingKey),
    masterPublicKey: word(masterPublicKey),
    address: encodeAddress({ masterPublicKey, viewingPublicKey: viewing.pubkey }),
  };
}

function shareableViewingKey(keys) {
  const spub = Babyjubjub.packPoint(keys.spendingPublicKey.map(BigInt)).toString('hex');
  const vpriv = keys.viewingPrivateKey.replace(/^0x/, '');
  return msgpack.encode({ vpriv, spub }).toString('hex');
}

const addressData = (keys) => ({
  masterPublicKey: BigInt(keys.masterPublicKey),
  viewingPublicKey: bytes(keys.viewingPublicKey),
});

async function transactOutput(sender, receiver, { random, senderRandom, value, token, memoText, outputType }) {
  WalletInfo.setWalletSource('zecswap vectors');
  const note = new TransactNote(
    addressData(receiver),
    addressData(sender),
    random.replace(/^0x/, ''),
    value,
    getTokenDataERC20(token),
    outputType,
    WalletInfo.walletSource,
    senderRandom ?? MEMO_SENDER_RANDOM_NULL,
    memoText,
  );
  const { blindedSenderViewingKey, blindedReceiverViewingKey } = getNoteBlindingKeys(
    bytes(sender.viewingPublicKey),
    bytes(receiver.viewingPublicKey),
    note.random,
    note.senderRandom,
  );
  const sharedKey = await getSharedSymmetricKey(bytes(sender.viewingPrivateKey), blindedReceiverViewingKey);
  const { noteCiphertext, noteMemo } = note.encryptV2(
    TXIDVersion.V2_PoseidonMerkle,
    sharedKey,
    BigInt(sender.masterPublicKey),
    note.senderRandom,
    bytes(sender.viewingPrivateKey),
  );
  return {
    note,
    ciphertext: [`0x${noteCiphertext.iv}${noteCiphertext.tag}`, ...noteCiphertext.data.map((d) => `0x${d}`)],
    blindedSenderViewingKey: hex(blindedSenderViewingKey),
    memo: hex(Buffer.from(noteMemo, 'hex')),
  };
}

async function receiveOutput(receiver, output) {
  const sharedKey = await getSharedSymmetricKey(bytes(receiver.viewingPrivateKey), bytes(output.blindedSenderViewingKey));
  const [ivTag, ...data] = output.ciphertext.map((word) => word.replace(/^0x/, ''));
  const tokens = { getTokenDataFromHash: async (_version, _chain, hash) => getTokenDataERC20(hash) };
  return TransactNote.decrypt(
    TXIDVersion.V2_PoseidonMerkle,
    undefined,
    addressData(receiver),
    { iv: ivTag.slice(0, 32), tag: ivTag.slice(32), data },
    sharedKey,
    output.memo,
    '0x',
    undefined,
    bytes(output.blindedSenderViewingKey),
    bytes(output.blindedSenderViewingKey),
    false,
    false,
    tokens,
  );
}

async function decryptRandom(viewingPrivateKey, encryptedBundle, shieldKey) {
  const sharedKey = await getSharedSymmetricKey(bytes(viewingPrivateKey), bytes(shieldKey));
  return ShieldNote.decryptRandom(encryptedBundle, sharedKey);
}

module.exports = {
  wallet,
  shareableViewingKey,
  transactOutput,
  receiveOutput,
  OutputType,
  Babyjubjub,
  deriveNodes,
  Mnemonic,
  decryptRandom,
  getSharedSymmetricKey,
  getPublicViewingKey,
  ShieldNote,
  ShieldNoteERC20,
  hex,
  bytes,
  word,
};
