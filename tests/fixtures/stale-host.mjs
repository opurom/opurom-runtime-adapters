// Test-only stub host: answers the handshake with the previous protocol id,
// then never answers invoke. Used to prove that the Rust side refuses a host
// whose frame schema is stale, instead of silently accepting missing fields.
import readline from 'node:readline';

const rl = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });

rl.on('line', (line) => {
  let msg;
  try {
    msg = JSON.parse(line);
  } catch {
    return;
  }
  if (msg.type === 'handshake') {
    process.stdout.write(
      JSON.stringify({ type: 'ready', ok: true, protocol: 'opurom-runtime-adapters/4' }) + '\n'
    );
  }
  // No reply for invoke/shutdown on purpose.
});
