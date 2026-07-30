/**
 * REST client.
 *
 * Covers everything that is request/response shaped: authentication, history
 * backfill, search, uploads. Live traffic goes over the WebSocket.
 *
 * The session token is held in memory and also set as an `HttpOnly` cookie by
 * the server. The cookie is what authenticates the WebSocket handshake and
 * `<img>` requests for attachments, neither of which can carry a header.
 */

import type { Attachment, Channel, Id, Message, ReadState, SearchHit, User } from './protocol.ts';

export class ApiError extends Error {
  status: number;
  code: string;
  constructor(status: number, code: string, message: string) {
    super(message);
    this.status = status;
    this.code = code;
  }
  /** True when the session is gone and the UI should return to the login screen. */
  get isAuthFailure(): boolean {
    return this.status === 401;
  }
}

let token: string | null = null;

export function setToken(t: string | null): void {
  token = t;
  if (t) localStorage.setItem('tc_token', t);
  else localStorage.removeItem('tc_token');
}

export function getToken(): string | null {
  if (token === null) token = localStorage.getItem('tc_token');
  return token;
}

async function request<T>(method: string, path: string, body?: unknown): Promise<T> {
  const headers: Record<string, string> = {};
  const t = getToken();
  if (t) headers['Authorization'] = `Bearer ${t}`;
  if (body !== undefined) headers['Content-Type'] = 'application/json';

  const res = await fetch(path, {
    method,
    headers,
    body: body === undefined ? undefined : JSON.stringify(body),
    // Send the session cookie too, so a token lost from memory still works.
    credentials: 'same-origin',
  });

  if (res.status === 204) return undefined as T;
  if (!res.ok) {
    // Error bodies are JSON, but a proxy or a crash can produce something else;
    // fall back to the status text rather than throwing inside the error path.
    let code = 'internal';
    let message = res.statusText;
    try {
      const parsed = (await res.json()) as { code?: string; message?: string };
      code = parsed.code ?? code;
      message = parsed.message ?? message;
    } catch {
      /* keep the fallback */
    }
    throw new ApiError(res.status, code, message);
  }
  return (await res.json()) as T;
}

type Session = { token: string; user: User };

export const api = {
  register: (handle: string, displayName: string, password: string) =>
    request<Session>('POST', '/api/register', {
      handle,
      display_name: displayName,
      password,
    }),

  login: (handle: string, password: string) =>
    request<Session>('POST', '/api/login', { handle, password }),

  logout: () => request<void>('POST', '/api/logout'),

  me: () => request<User>('GET', '/api/me'),

  updateMe: (patch: { display_name?: string; status?: string }) =>
    request<User>('PATCH', '/api/me', patch),

  users: () => request<User[]>('GET', '/api/users'),

  channels: () => request<Channel[]>('GET', '/api/channels'),

  browseChannels: () => request<Channel[]>('GET', '/api/channels/browse'),

  createChannel: (name: string, isPrivate: boolean, topic = '', members: Id[] = []) =>
    request<Channel>('POST', '/api/channels', { name, private: isPrivate, topic, members }),

  updateChannel: (id: Id, patch: { name?: string; topic?: string; archived?: boolean }) =>
    request<Channel>('PATCH', `/api/channels/${id}`, patch),

  joinChannel: (id: Id) => request<Channel>('POST', `/api/channels/${id}/join`),

  leaveChannel: (id: Id) => request<void>('POST', `/api/channels/${id}/leave`),

  members: (id: Id) => request<Id[]>('GET', `/api/channels/${id}/members`),

  /**
   * Add people to a channel. This is the only way into a private one, so the
   * server requires the caller to already be a member.
   *
   * Resolves with just the ids that were actually added — anyone already in the
   * channel is absent rather than an error.
   */
  addMembers: (id: Id, users: Id[]) =>
    request<{ added: Id[] }>('POST', `/api/channels/${id}/members`, { users }),

  removeMember: (id: Id, user: Id) =>
    request<void>('DELETE', `/api/channels/${id}/members/${user}`),

  /** One page of history, newest first. Pass the previous page's cursor to page back. */
  history: (channel: Id, before?: Id | null, limit = 50) => {
    const q = new URLSearchParams({ limit: String(limit) });
    if (before) q.set('before', before);
    return request<{ messages: Message[]; next_cursor: Id | null }>(
      'GET',
      `/api/channels/${channel}/messages?${q}`,
    );
  },

  thread: (root: Id) => request<Message[]>('GET', `/api/threads/${root}`),

  openDm: (users: Id[]) => request<Channel>('POST', '/api/dm', { users }),

  markRead: (channel: Id, upTo: Id) =>
    request<ReadState>('POST', `/api/messages/${channel}/read`, { up_to: upTo }),

  search: (q: string, opts: { channel?: Id; author?: Id; limit?: number } = {}) => {
    const params = new URLSearchParams({ q });
    if (opts.channel) params.set('channel', opts.channel);
    if (opts.author) params.set('author', opts.author);
    if (opts.limit) params.set('limit', String(opts.limit));
    return request<SearchHit[]>('GET', `/api/search?${params}`);
  },

  /** Upload a file and get back a staged attachment to reference in a message. */
  upload: async (file: File): Promise<Attachment> => {
    const form = new FormData();
    form.append('file', file);
    const headers: Record<string, string> = {};
    const t = getToken();
    if (t) headers['Authorization'] = `Bearer ${t}`;

    const res = await fetch('/api/uploads', {
      method: 'POST',
      headers,
      body: form,
      credentials: 'same-origin',
    });
    if (!res.ok) {
      let message = res.statusText;
      try {
        message = ((await res.json()) as { message?: string }).message ?? message;
      } catch {
        /* keep the fallback */
      }
      throw new ApiError(res.status, 'bad_request', message);
    }
    return (await res.json()) as Attachment;
  },
};

/** URL for an attachment's bytes. Authenticated by the session cookie. */
export function fileUrl(id: Id): string {
  return `/api/files/${id}`;
}
