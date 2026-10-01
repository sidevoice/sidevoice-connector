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

  // Install keys (the core's runtime could not be had).
  'install.no-bundle': 'There is no core build for this platform and uv is not installed: install uv (https://docs.astral.sh/uv/) and try again.',
  'install.network': 'The core could not be downloaded: the network is not reachable.',
  'install.proxy': 'The core could not be downloaded: a proxy re-signs HTTPS and its certificate is not trusted.',

  // The launcher and the node service (§4.2). `node.stopped` also reaches agents, so it stays plain English.
  'node.stopped': 'Sidevoice is stopped on this computer: start it from the app or with `sidevoice service start`.',
  'connector.not-started': 'The Sidevoice connector did not start; see ~/.sidevoice/connector.log.',
  'service.not-loaded': 'The Sidevoice service is installed but its service manager did not start it ({detail}). Start it with `sidevoice service start`, or reinstall it with `sidevoice install --no-agents --service`.',
  'service.executable-missing': 'The Sidevoice service points at a program that is not there any more ({detail}). Reinstall: `sidevoice install --no-agents --service`.',
  'service.permission-denied': 'The Sidevoice service cannot run its program: permission denied ({detail}).',
  'service.start-limit': 'The Sidevoice service failed too many times in a row and its service manager stopped trying ({detail}). Retry with `sidevoice service restart`.',
  'service.unload-failed': 'The service manager did not unload the Sidevoice service ({detail}); nothing was deleted.',
  'service.no-installation': 'Sidevoice is not installed on this computer: run `sidevoice install` first.',
  'service.failed': 'The Sidevoice service command failed.',
  'service.killed': 'Processes that did not stop in time were killed: {pids}.',
  'service.linger-reason': 'Linux runs your services only while you are logged in. To keep Sidevoice running without a session (after a reboot, over SSH), the machine\'s owner can enable lingering for your user — it is a system setting, so Sidevoice does not change it:',
  'service.usage': 'usage: sidevoice service <install|uninstall|start|stop|restart|status> [--json]',
  'service.state.absent': 'Sidevoice is not installed on this computer.',
  'service.state.not-installed': 'Sidevoice is installed but does not start at login (no service). Enable it with `sidevoice service install`.',
  'service.state.stopped-by-person': 'Sidevoice is stopped until you start it (`sidevoice service start`) or log in again.',
  'service.state.stopped': 'The Sidevoice service is running; its core is stopped.',
  'service.state.starting': 'The Sidevoice core is starting…',
  'service.state.running': 'Sidevoice is running ({service}).',
  'service.state.backoff': 'The Sidevoice core failed and is being retried.',
  'service.state.failed': 'The Sidevoice core failed and is no longer retried: `sidevoice service restart` tries again.',
  'service.state.service-failed': 'The Sidevoice service is not running.',
  'service.reason.executable-missing': 'its program is missing',
  'service.reason.permission-denied': 'permission denied',
  'service.reason.start-limit': 'too many failed starts',
  'service.reason.not-loaded': 'not loaded by the service manager',

  // Pairing.
  'pair.core-restarted': 'This machine\'s core was restarted with the new pairing.',
  'pair.failed': 'Pairing with the room failed.',
  'pair-device.failed': 'No pairing code could be had from this machine\'s core.',
  'pair-device.local-only': 'This code only works on this computer: the machine is not reachable from other devices. To use it from elsewhere, connect it to your room: in the room, "Emparejar máquina", then here  sidevoice pair <room-url> <code>.',
};
