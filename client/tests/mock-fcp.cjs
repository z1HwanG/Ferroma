// A throwaway FCP mock for smoke-testing the CLI end to end (not part of the client).
const http = require("http");

const folders = [
  { id: 5, name: "INBOX", special_use: null, message_count: 1, unseen_count: 1, uid_validity: 1, uid_next: 2 },
  { id: 6, name: "Sent", special_use: "\\Sent", message_count: 0, unseen_count: 0, uid_validity: 1, uid_next: 1 },
];

const routes = {
  "POST /api/v1/client/auth/login": () => [
    200,
    {
      access_token: "access-1",
      refresh_token: "rt_1",
      token_type: "Bearer",
      expires_in: 3600,
      device_id: 12,
      user: { id: 7, email: "alice@example.com" },
    },
  ],
  "POST /api/v1/client/auth/refresh": () => [
    200,
    { access_token: "access-2", refresh_token: "rt_2", token_type: "Bearer", expires_in: 3600 },
  ],
  "POST /api/v1/client/auth/logout": () => [200, {}],
  "GET /api/v1/client/account": () => [
    200,
    {
      user: { id: 7, email: "alice@example.com" },
      protocol_version: 1,
      min_protocol_version: 1,
      server_version: "0.1.0-test",
      server_hostname: "mock",
      limits: { max_message_size: 26214400, max_recipients: 100, attachment_chunk_size: 1048576, sync_page_size: 500 },
      features: ["sync", "events", "drafts", "attachments", "devices", "search"],
    },
  ],
  "GET /api/v1/client/mailboxes": () => [
    200,
    { mailboxes: [{ id: 3, address: "alice@example.com", display_name: "Alice", is_primary: true, folders }] },
  ],
  "GET /api/v1/client/sync": (url) => {
    // A real server only returns a folder's own changes in that folder's stream.
    if (url.includes("folder_id=5")) {
      return [200, { next_cursor: "1", has_more: false, changes: [{ type: "message_created", seq: 1, message_id: 4821, uid: 1 }] }];
    }
    if (url.includes("folder_id=6")) {
      return [200, { next_cursor: "1", has_more: false, changes: [] }];
    }
    return [200, { next_cursor: "2", has_more: false, changes: [{ type: "folder_created", seq: 2, folder_id: 6, name: "Sent" }] }];
  },
  "GET /api/v1/client/messages/4821": () => [
    200,
    {
      id: 4821,
      uid: 1,
      folder_id: 5,
      subject: "Invoice for September",
      from: { address: "bob@example.net", name: "Bob" },
      to: [{ address: "alice@example.com" }],
      snippet: "Hi Alice, attached is the invoice…",
      flags: "seen",
      size_bytes: 24831,
      has_attachments: false,
      attachment_count: 0,
      internal_date: "2026-09-16T09:12:44Z",
      sent_at: "2026-09-16T09:12:31Z",
      rfc_message_id: "<20260916091231.7f3a@example.net>",
      text_body: "Hi Alice,\n\nAttached is the invoice for September.\n\nBob",
      headers: [{ name: "Subject", value: "Invoice for September" }],
    },
  ],
  "POST /api/v1/client/messages": () => [200, { message_id: 4900, queued: 1, recipients: ["bob@example.net"] }],
  "GET /api/v1/client/devices": () => [200, { devices: [{ id: 12, device_uid: "dev", name: "Smoke", platform: "windows", client_version: "0.1.0", protocol_version: 1, revoked: false, last_seen_at: "2026-09-16T12:00:00Z", last_ip: "203.0.113.44" }] }],
  "GET /api/v1/client/search": () => [200, { items: [], total: 0, limit: 50, offset: 0 }],
};

const seen = [];
http
  .createServer((req, res) => {
    let body = "";
    req.on("data", (chunk) => (body += chunk));
    req.on("end", () => {
      const path = req.url.split("?")[0];
      seen.push(`${req.method} ${req.url}`);
      const handler = routes[`${req.method} ${path}`];
      const [status, payload] = handler ? handler(req.url, body) : [404, { error: { code: "not_found", message: path } }];
      const text = JSON.stringify(payload);
      res.writeHead(status, { "Content-Type": "application/json", "Content-Length": Buffer.byteLength(text) });
      res.end(text);
    });
  })
  .listen(8940, "127.0.0.1", () => console.log("mock FCP on http://127.0.0.1:8940"));
