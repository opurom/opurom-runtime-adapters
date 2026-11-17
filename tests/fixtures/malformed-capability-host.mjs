// Test-only stub host: completes the handshake with the current protocol id,
// then answers capability calls with deliberately malformed frames. Used to
// prove that the Rust side rejects bad envelopes instead of coercing them.
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
      JSON.stringify({ type: 'ready', ok: true, protocol: 'opurom-runtime-adapters/5' }) + '\n'
    );
    return;
  }
  if (msg.type === 'capability') {
    const method = msg.call && msg.call.method;
    if (method === 'get') {
      // A success reply must carry the value slot.
      process.stdout.write(JSON.stringify({ type: 'capability_result' }) + '\n');
    } else {
      // An error reply must carry a string code slot.
      process.stdout.write(JSON.stringify({ type: 'capability_error', message: null }) + '\n');
    }
  }
  // No reply for invoke/shutdown on purpose.
});
