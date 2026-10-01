/** English, the fallback bundle: every key a person can be shown has its words here first. */
export const en = {
  // The core's directory and socket (§4.1).
  'identity.unsafe-directory': 'The Sidevoice core directory {path} is not safe to use ({why}): it must be a directory owned by you with no access for anyone else (chmod 700).',
  'peer.uid-mismatch': 'The socket {path} is not a socket owned by you; Sidevoice will not talk to it.',
  'core.socket-missing': 'The Sidevoice core is not listening at {path} ({why}).',

  // Why the core did not come up (§4.2 cause keys).
  'identity.unreadable': 'The core could not read this machine\'s identity.',
  'bind.port-in-use': 'The core\'s port is taken by another program.',
  'import.missing-module': 'The core is missing a part it needs ({detail}).',
  'launch.missing-executable': 'The core\'s program is not where it was installed ({detail}).',
  'launch.permission': 'The core\'s program cannot be run: permission denied ({detail}).',
  'launch.exited': 'The core stopped while starting (exit {detail}).',
  'hang': 'The core stopped answering.',
  'ready.timeout': 'The core did not become ready in time.',
};
