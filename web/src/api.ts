// Small shared API client. Factors the ubiquitous
//   fetch(url, { headers: authHeaders() })
//   fetch(url, { method, headers: { ...authHeaders(), "content-type": "application/json" }, body: JSON.stringify(body) })
// boilerplate into a few typed helpers. Each returns the raw Response, so existing call sites keep
// their `if (!res.ok) uiAlert(await apiError(res))` handling unchanged — the helpers only remove the
// header/JSON plumbing. HTTP header ordering is not observable, so these are byte-for-byte equivalent
// to the hand-written fetches they replace.

export type ApiHeaders = Record<string, string>;

// GET with auth headers.
export function apiGet(url: string, headers: ApiHeaders = {}): Promise<Response> {
  return fetch(url, { headers });
}

// Shared body-carrying request. When `body` is provided it is JSON-encoded and a JSON content-type is
// added (matching the codebase convention); when omitted, neither is set (matching body-less POSTs).
function apiSend(method: string, url: string, body?: unknown, headers: ApiHeaders = {}): Promise<Response> {
  const hasBody = body !== undefined;
  return fetch(url, {
    method,
    headers: hasBody ? { ...headers, "content-type": "application/json" } : { ...headers },
    body: hasBody ? JSON.stringify(body) : undefined,
  });
}

export const apiPost = (url: string, body?: unknown, headers: ApiHeaders = {}): Promise<Response> =>
  apiSend("POST", url, body, headers);
export const apiPut = (url: string, body?: unknown, headers: ApiHeaders = {}): Promise<Response> =>
  apiSend("PUT", url, body, headers);
export const apiPatch = (url: string, body?: unknown, headers: ApiHeaders = {}): Promise<Response> =>
  apiSend("PATCH", url, body, headers);
export const apiDelete = (url: string, headers: ApiHeaders = {}): Promise<Response> =>
  apiSend("DELETE", url, undefined, headers);
