# Hybrid mode: local Outlook MCP and Microsoft 365 together

Read this when both sources are usable (SKILL.md "Choosing the mode").
Call details: local tools in SKILL.md ("Local Outlook MCP" onward),
M365 tools in `references/m365.md`. Everything is read-only.

The idea: do the **wide** work (listing, folders, keyword scans,
attachment text) on the local snapshot, where a call returns 100 messages
with folder names; do the **narrow** work (mail newer than the snapshot,
recipients, thread id, reply checks, calendar) on M365, where data is live
but a call returns 25 messages and recipients cost one read per message.

## 0. Start every hybrid task the same way (one parallel block)

- local `store_status` → `built` (snapshot time), `service.building`,
  `last_error`
- M365 `get_me` (first task of the session only)

Then:
- `last_error` set or a `warning` on a local result: tell the user the
  local data is old and why; treat the snapshot as ending at `built`
  anyway (the delta below covers the rest).
- `service.building` true: do not wait. The delta covers the gap.
- Never call `refresh` in hybrid mode unless the user asks for it.

## 1. The window split

For a question about a period (today, since Monday, last week):

- **Snapshot part**: `since` = period start, `until` = `built`, on the
  local tools.
- **Delta part**: `afterDateTime` = `built` minus 10 minutes (overlap), on
  M365, no `folderName` (all folders), `limit: 25`, page to the end.
  When the period ends before `built`, there is no delta part.

Run the snapshot listing and the delta search **in the same block**.

## 2. Merging and dedupe

1. Overlap: items from the 10-minute overlap appear twice. Treat a local
   and an M365 item as the same message when the sender address matches,
   the subject matches (ignoring `RE:`/`FW:` prefixes and case), and the
   sent times are within 2 minutes. When that still leaves doubt (same
   sender, same subject, close times), confirm with the Internet message
   id: local `get_message` → `internet_message_id`, M365 search result →
   `internetMessageId`.
2. Conflicts: M365 is live, so its read state, subject and existence win.
   A message the snapshot has but M365 no longer returns for the same
   window may have been deleted or moved since `built`: say so only if it
   matters for the answer.
3. Folder: keep the local `folder`. Delta items have none: label them
   "new since the snapshot".
4. Apply the user's ignore rules and the noise filter to the merged list.

## 3. Triage: important, urgent or "needs me"

1. Step 0, then the window split for "today" (local day start in UTC).
2. Snapshot part: local `list_messages` with `since` (no `folder`, so rules
   folders are included), `limit: 100`, page by `offset` until `total`.
   Delta part: M365 `outlook_email_search` as in step 1. Same block.
3. Merge (section 2). Drop noise and say in one line how many messages and
   why (automated senders, alert bursts counted as one item, marketing,
   ignored senders). Read a local `get_message` body only where the
   preview is not enough to judge an alert.
4. Shortlist the human or decision-relevant items (usually ≤ 10).
5. For each shortlisted item, **in one parallel block**:
   - M365 `outlook_email_search` for the message (subject words +
     `sender`, or `afterDateTime` near its sent time) to get its `uri`, if
     it came from the snapshot;
   - then `read_resource` → To/Cc (is the user addressed directly or only
     in Cc?), `conversationId`.
6. Reply check per item (M365, parallel across items):
   - `outlook_email_search` with `recipient` = the inbound sender and
     `afterDateTime` = inbound time, no `folderName`, no `order`; keep hits
     sent by the user with the same `conversationId` → "you replied at
     <time>".
   - `outlook_email_search` with `query` = the thread keyword and
     `afterDateTime` = inbound time; read the newest hit with the same
     `conversationId`; a closing message ("done", "handled", "טופל",
     "✅", "received") → "closed by <name>".
   - Neither → "waiting on you".
   Without M365 for a step (it failed), use the local reply check in
   `references/triage-and-threads.md` for the remaining items and say so.
7. Rank and report as in `references/m365.md` (To above Cc, direct asks,
   blockers, deadlines within about two working days, security notices
   first), with the source line from SKILL.md rule 4.

## 4. Folder work

- "What folders / how many unread": local `list_folders` with
  `limit: 100` (page if `total` is larger). Report `cached_unread` vs
  `unread_count` as SKILL.md "Folders" says. Mention that mail newer than
  `built` is not counted yet, and give the delta count from M365 if the
  user needs a current number.
- "What is in folder X (today / recently)": local `list_messages` with
  `folder` and `since`. For mail newer than `built`, the delta search
  cannot filter by folder name reliably (`folderName` with a date and a
  query is rejected, and a delta item carries only a folder id): list
  delta items that plausibly belong (same senders as the folder's recent
  mail) as "new since the snapshot, probably in X", or wait for the next
  snapshot if the user needs exact placement.
- Two folders with the same name: as SKILL.md "Folders" says, show both
  with their `parent` and use the guid.

## 5. A whole thread

1. Local `search_messages` with the topic word (Inbox and Sent Items, one
   search per variant) for the snapshot period; M365 `outlook_email_search`
   with the same word for the delta, in the same block.
2. Pick the message the user means; M365 `read_resource` it for its
   `conversationId`.
3. M365 `read_resource` the other candidates (parallel) and keep those
   with the same `conversationId`; same-topic items with another id are a
   related thread, mention them separately.
4. Read the oldest message in full: local `get_message` with
   `max_chars: 80000` if it is in the snapshot (cheaper, plain text),
   otherwise M365.
5. Report as in `references/m365.md` "Analyzing a thread".

## 6. Attachments

1. Find the message's attachments: local `list_attachments` if the message
   is in the snapshot, else M365 `read_resource` on the message.
2. Read the one file asked about with local `get_attachment` (extracted
   text for docx, xlsx, pptx, pdf; see SKILL.md "Attachments").
3. Local says the file is not on this Mac, or the message is only in the
   delta → M365 `read_resource` on the attachment `uri`.
4. "What did I send": the user's own sent message, on either source.

## 7. "Everything" questions and completeness

When the user wants every message of a period (counts, "all mail from X",
an audit), check the local cache is complete before answering:

- M365 `outlook_email_search` with the same window and filters,
  `limit: 1`, and read `totalResultCount`.
- Compare with local `total` for the snapshot part plus the delta count.
- Close (within a few messages): answer from the merged list.
- M365 clearly higher: the local cache is missing folders or older mail.
  Take the whole period from M365 (page to the end) and say why.

## 8. Calendar and people

- Calendar: M365 `outlook_calendar_search` (live, attendees, transcripts);
  local `list_events` only if M365 fails.
- People: M365 `search_people`; local `search_people` if M365 fails.

## Common mistakes in hybrid mode

- Waiting for a local rebuild, or calling `refresh`, when M365 can supply
  the mail newer than `built`.
- Paging the whole day through M365 25 at a time when the local snapshot
  covers it at 100 per call with folder names.
- Calling M365 `read_resource` for every message instead of the shortlist.
- Running the snapshot listing and the delta search one after the other
  instead of in one block.
- Reporting a delta item as being in a folder: it has no folder name yet.
- Counting overlap-window messages twice.
- Mixing sources silently: always give the one-line source note.
