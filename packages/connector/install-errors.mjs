import { keyed } from './i18n.mjs';

function errorChain(value) {
  const pending = [value], found = [], seen = new Set();
  while (pending.length && found.length < 16) {
    const current = pending.shift();
    if (!current || (typeof current !== 'object' && typeof current !== 'string') || seen.has(current)) continue;
    seen.add(current);
    found.push(typeof current === 'string' ? current : `${current.code || ''} ${current.message || ''}`);
    if (current.cause) pending.push(current.cause);
    if (Array.isArray(current.errors)) pending.push(...current.errors);
  }
  return found.join('\n');
}

export function classifyInstallFailure(error, status = null, { proxyText = true } = {}) {
  const text = errorChain(error);
  const proxyCode = /\b(?:ERR_PROXY_CONNECTION_FAILED|ERR_TUNNEL_CONNECTION_FAILED|CERT_HAS_EXPIRED|DEPTH_ZERO_SELF_SIGNED_CERT|SELF_SIGNED_CERT_IN_CHAIN|UNABLE_TO_GET_ISSUER_CERT_LOCALLY|UNABLE_TO_VERIFY_LEAF_SIGNATURE|ERR_TLS_CERT_ALTNAME_INVALID|ERR_TLS_CERT_INVALID)\b/i.test(text);
  const proxyMessage = proxyText && /UnknownIssuer|certificate verify failed|CERTIFICATE_VERIFY_FAILED|self[- ]signed certificate|unable to get local issuer|unable to verify the first certificate/i.test(text);
  if (status === 407 || proxyCode || proxyMessage) return 'proxy';
  if (/\b(?:ENOSPC|EDQUOT)\b|no space left on device|disk (?:is )?full|disk quota exceeded|quota exceeded/i.test(text)) return 'disk';
  if (/\b(?:ECONNREFUSED|ECONNRESET|EHOSTUNREACH|ENETUNREACH|ENOTFOUND|EAI_AGAIN|ETIMEDOUT|UND_ERR_CONNECT_TIMEOUT)\b|dns error|failed to lookup address|could not connect|connection refused|connection reset|error sending request|timed out|network is unreachable|tcp connect error/i.test(text)) return 'network';
  return null;
}

/** Map operating-system storage failures at the install boundary while preserving already keyed refusals. */
export function normalizeInstallFailure(error) {
  if (error?.key) return error;
  return classifyInstallFailure(error) === 'disk' ? keyed('install.disk') : error;
}

/** A download failure is transport availability unless it is an explicit proxy/TLS interception failure. */
export function downloadFailure(error, status = null) {
  return keyed(classifyInstallFailure(error, status) === 'proxy' ? 'install.proxy' : 'install.network');
}
