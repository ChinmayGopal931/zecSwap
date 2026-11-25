const { HDNodeWallet, Mnemonic, sha256, toUtf8Bytes } = require('ethers');

const DEPOSIT_ACCOUNTS = 1000;
const seeds = [
  Mnemonic.fromPhrase('test test test test test test test test test test test junk').computeSeed(),
  Mnemonic.fromPhrase('abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about').computeSeed(),
  sha256(toUtf8Bytes('zecswap deposit vectors')),
];
const vectors = [];
for (const seed of seeds) {
  const root = HDNodeWallet.fromSeed(seed);
  for (const [network, coin] of [['mainnet', 60], ['testnet', 1]]) {
    for (const [account, index] of [[0, 0], [0, 1], [0, 19], [1, 0], [7, 3]]) {
      const path = `m/44'/${coin}'/${DEPOSIT_ACCOUNTS + account}'/0/${index}`;
      vectors.push({ seed, network, account, index, path, address: root.derivePath(path).address });
    }
  }
}
console.log(JSON.stringify(vectors, null, 2));
