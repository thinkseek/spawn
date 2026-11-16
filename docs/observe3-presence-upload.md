# observe3 presence upload: delivery of spawn-agent-presence v1

`scripts/observe3/presence_upload.py` sends the document that `agent_presence.py` writes (`docs/observe3-agent-presence.md`) to the observe3 receiver by one outbound HTTPS POST. Nothing on the fleet host listens, and no inbound port is opened.

## Off by default
It sends nothing unless all of these hold:
- `~/.config/spawn-agent-presence/upload.json` contains exactly `{"enabled": true}`: the JSON literal `true`, not `1` or `1.0`, with no other key and no duplicate key (rev-e F1);
- `~/.config/spawn-agent-presence/ingest-token` exists: a regular file owned by the user, not a link, mode 0600 or 0400, at most 512 bytes;
- the exporter's off switch `~/.config/spawn-agent-presence/disabled` is absent.

The token must be 32-256 characters of `[A-Za-z0-9-_.~]` with at least 16 distinct characters. The production path refuses `TEST-ONLY-` values and placeholder words (`example`, `placeholder`, `changeme`, `dummy`, `sample`, `synthetic`, `not-a-credential` and the others in `PLACEHOLDER_MARKERS`), the same list the receiver refuses. Only the tests accept `TEST-ONLY-` tokens, through a flag that the program's entry point never sets.

**No token exists yet.** Issuing one is an owner decision (a dedicated, write-only bearer token for this one route).

## What, where, how
| | |
|---|---|
| Target | `POST https://www.flora.chat/observe3/ingest/agent-presence`. Fixed in the code, not configurable. Port 443; TLS verified with the system trust store, hostname checked |
| Headers | `Authorization: Bearer <token>`, `Content-Type: application/json`, `Content-Length`. No `Origin`, no query string, no token in any URL |
| Body | exactly the bytes of `~/.local/state/spawn-agent-presence/presence.json`: at least 1 and at most 16384 bytes, re-checked with the exporter's own validator first. A duplicate JSON key at any level makes it `invalid_document`, so no byte leaves that the validator did not check (cc-astra A91). Never the id map, the lock or any other file |
| Order | strictly increasing `generated_utc`. Once the receiver acknowledged a document (200, 204 or 409) and that was saved, it and any older or equal one are `already_sent`. This is not exactly-once transport: a lost answer, an interrupted run or a failed state write can send the same document again, and the receiver's replay check refuses the copy. A `state_write_error` is no proof that nothing was sent |
| Freshness | sent only while at most 90 s old (the receiver refuses older than 120 s) and at most 5 s ahead (the receiver's skew limit is 30 s) |
| Rate | at most one request per 10 s (the receiver's producer budget is 4, +1 per 5 s); the timer runs every 15 s |
| Redirects | never followed (any 3xx is `rejected`) |
| Response | the body is read (at most 1 KiB) and dropped; nothing is logged but one fixed word |
| Through the Mac's proxy | the fleet host resolves the name to the Mac's transparent proxy range; TLS still verifies end to end (checked 2026-11-16 17:40: HEAD /observe3/ gave 200, TLS 1.3) |

## Result words and back-off
| Receiver answer | Word | Next attempt | Marks the document sent |
|---|---|---|---|
| 204 / 200 | `sent` | next document | yes |
| 409 (replay or old) | `replay_or_old` | next document | yes: never resent |
| 401 | `unauthorized` | after 600 s | no |
| 429 | `rate_limited` | after 30 s | no |
| 5xx | `server_error` | after 30 s | no |
| no answer, TLS failure | `network_error` | after 30 s | no |
| any other (3xx, 400, 403, 404, 405, 411, 413, 415, 422) | `rejected` | after 300 s | no |

Local words: `off`, `disabled`, `token_refused`, `no_document`, `invalid_document`, `too_old`, `already_sent`, `too_soon`, `backoff`, `busy`, `state_dir_refused`, `state_write_error`, `disk_full`. The journal gets one line `presence_upload: <word>` except for the quiet words `sent`, `off`, `disabled`, `already_sent` and `too_soon`.

Files are opened without following links and without blocking. A FIFO or device at a path is refused, never waited on. From the environment it uses only `HOME`, to find its files. OpenSSL would also honour `SSL_CERT_FILE`/`SSL_CERT_DIR`, so the unit clears both and the system trust store is used.

Its only file of its own is `upload-state.json` (0600, written atomically and the folder fsynced) in the exporter's folder: the last sent `generated_utc`, the back-off time and the last attempt time. There is also a lock, `upload.lock`.

## Compatibility with the receiver (observe3 commit 61dc1f3, inactive)
Checked locally on 2026-11-16 17:50. The website stream's `Receiver` (read in place, behind a 127.0.0.1 test server, TEST-ONLY token) received this uploader's requests:
- an **R4 document (with `id_epoch`)**: **422 invalid**. The receiver is pinned to R2 and rejects unknown fields; this is expected until it is updated on purpose;
- the same document **without `id_epoch`**: **204 accepted**. Headers, body, path and size all match;
- the same bytes again: **409 replay**;
- a wrong token: **401**.

So the only change the receiver needs is the R2 -> R4 delta: the document field `id_epoch` (16 lowercase hex characters, or null when `agents` is empty), identity (`id_epoch`, `id`), and a relay reset on an epoch change. See the exporter's contract.

## Installing the unit (with the push; still off)
The push installs the script in the controller tree. Copy the two unit files to `~/.config/systemd/user/` and run `systemctl --user daemon-reload`. Do not enable the timer until the steps below are authorized. Even with the timer running, the uploader is `off` while `upload.json` is absent.

## Turning it on (not authorized; for later)
Only after: the exporter is reviewed and live; the receiver accepts R4 and is reviewed; the owner approves the credential and the publication granularity; and the parent has opened the nginx route. Then:
1. Put the issued token in `~/.config/spawn-agent-presence/ingest-token` (0600).
2. Write `{"enabled": true}` to `upload.json`.
3. `systemctl --user enable --now spawn-agent-presence-upload.timer`.

Rollback: remove `upload.json`. The next run is `off`, and the receiver marks the source stale after 60 s by itself.
