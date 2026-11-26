# Triage and threads (local backend)

This file is the **local-only** version. In hybrid mode use
`references/hybrid.md`; in M365-only mode use the triage and thread
sections of `references/m365.md`. The principles are the same in all
three; the calls differ.

Workflows for "what's important", "what's urgent", "what needs me",
"analyze this thread" and "did I answer X". The server has no thread id, no
recipients and no replied flag, so build them from subject, keyword and
sent-folder searches as below.

The sent folder is `Sent Items` in most mailboxes. A folder name that does
not exist is an error: when `Sent Items` is refused, call `list_folders`,
find the sent folder by its name there and use its guid.

## 1. Triage: important, urgent or "needs me"

1. Check freshness first: `store_status`; `refresh` when
   `store_changed_since_build` is true and the user asks about today or
   recent mail (see "Freshness" in SKILL.md).
2. List candidates: `list_messages` on `Inbox` with `since` set to the
   local-day boundary in UTC (`unread_only: true` when the user says
   unread). Use `limit: 100` and page; the default of 20 hides mail.
3. Drop noise before ranking. Leave these out unless the user asks for
   them, and say in one line how many were left out and why:
   - automated senders: `noreply`, `no-reply`, `notifications@`,
     `notice@`, `health@aws.com`, `updates-noreply@linkedin.com`, GitHub
     release notices;
   - marketing and newsletters (subject tags such as `(פרסומת)`,
     "webinar", "last chance", event promotions);
   - any sender the user asked to ignore.
4. Resolve notifications to the thread behind them. A Google Drive or
   Sheets "shared with you" / "share request", a monday.com or Jira
   mention, a Lucid ownership notice points at work in another thread.
   Search that thread by its keyword (the file or item name) and judge
   the thread, not the notification.
5. **Reply check: run it for every item before calling it open.**
   - Search `Sent Items` with the thread's keyword (subject without
     `RE:`, `FW:`, `Fwd:`, `TR:`, `AW:`), `since` = the inbound
     message's `sent` time. A hit from the user means "you replied at
     <time>".
   - Read the newest message in the same thread from anyone
     (`search_messages` with the keyword, then `get_message` on the most
     recent). A status update, "done", "sent", "✅" or "thanks, received"
     closes the item even when the user never wrote.
   - Only items with no reply from the user and no closing message are
     "waiting on you".
6. Rank: direct asks to the user by name ("@Name please advise",
   "please approve", "please confirm") and items with a deadline first;
   then customer and partner mail; then FYI. "Urgent" means an explicit
   urgency word, a deadline within about two working days, a security or
   abuse notice, or an approval someone is blocked on.
7. Report per item: subject, sender, local time, the ask in one line, and
   its state (`waiting on you`, `you replied <time>`, `closed by <name>`).
   Never list an item as waiting on the user without step 5.

`is_read` reflects the snapshot: a message the user read after the last
build still shows unread. A read message can still be open, and an unread
one can be closed.

## 2. Analyzing a whole thread

1. Find every message: `search_messages` with the topic's distinctive
   word, not the exact subject (threads get renamed: "Budget draft"
   becomes "Budget 2027 – final numbers"). Run
   one search per variant and merge by `guid`. Search both `Inbox` and
   `Sent Items`; no hit in `Sent Items` means the user has not written in
   the thread, and say so.
2. Read the oldest message in full (`max_chars: 80000`): its quoted
   history often holds earlier rounds that later replies cut off. Then
   read each later message for its new top part only.
3. Report in this order:
   - one line: what the thread is about and what is open now;
   - a dated timeline (local time), one line per turn: who, what they
     added or asked;
   - the open question and who owes the answer, by name;
   - deadlines, and any that contradict each other;
   - whether the user has written in the thread.
4. Offer a recommendation or a draft reply only when the user asks or the
   open question is addressed to them; keep it separate from the facts.

## 3. Attachments

- `list_attachments` returns embedded body images beside the files, with
  a `guid` per entry: tell them apart as "Attachments" in SKILL.md says.
- To answer a question about a file's content, call `get_attachment` with
  the `guid` of that one entry and follow "Attachments" in SKILL.md. Read
  only the attachment asked about, not every entry and not the embedded
  images, unless the user asks for them.
- "What did I send": see "Attachments" in SKILL.md.
- In triage, read an attachment with `get_attachment` only when the
  message cannot be judged without it.
