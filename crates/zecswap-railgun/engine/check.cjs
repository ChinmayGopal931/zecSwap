const readline = require('readline');
const { wallet, decryptRandom, ShieldNote, word } = require('./engine.cjs');

async function check(note) {
  const receiver = await wallet(note.seed, note.index);
  if (receiver.address !== note.address) throw new Error(`the engine's address is ${receiver.address}`);
  const random = await decryptRandom(receiver.viewingPrivateKey, note.encryptedBundle, note.shieldKey);
  if (`0x${random}` !== note.random) throw new Error(`decrypted random 0x${random}, expected ${note.random}`);
  const npk = word(ShieldNote.getNotePublicKey(BigInt(receiver.masterPublicKey), note.random));
  if (npk !== note.npk) throw new Error(`the engine's npk is ${npk}, the note's ${note.npk}`);
}

async function main() {
  let checked = 0;
  for await (const line of readline.createInterface({ input: process.stdin })) {
    if (!line.trim()) continue;
    await check(JSON.parse(line));
    checked += 1;
  }
  if (checked === 0) throw new Error('no notes on stdin');
  console.log(`the engine decrypted all ${checked} notes`);
}

main().catch((error) => {
  console.error(error.message);
  process.exit(1);
});
