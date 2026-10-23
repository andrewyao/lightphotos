# One writer per folder

Status: approved. Fixes B3 (headless `lightphotos-mcp` and the app on one
folder). The code is in `sidecar/lock.rs` (the lock, `FolderKey`) and
`persist/folder_store/locks.rs` (the app's locks).

## 1. Goal

Two programs must not write one folder's sidecars at the same time.

| Writer | Writes | Guarded by this design |
|---|---|---|
| LightPhotos app (this process) | sidecars, groups, coverage, thumbnails | yes |
| A second LightPhotos instance | same | yes |
| Headless `lightphotos-mcp --folder` | sidecars, coverage | yes |
| Attached `lightphotos-mcp --attach` | nothing (goes through the app) | not needed |
| Other tools (sync clients, Finder) | anything | no (out of scope) |

## 2. Rules

1. **The app's own writes are never refused.** The app either holds the
   folder's lock, or it opened the folder read-only and writes nothing to
   it. No write that the app has accepted from the user fails because of
   the lock.
2. **Only an outside writer is turned away.** A headless MCP command that
   finds the lock held fails with "use `--attach`". A second app instance
   opens the folder read-only.
3. **Read-only is decided once, at open, and is shown.** It lasts for the
   visit. Every edit control is off. A read-only visit writes nothing
   into the folder *from the user*.
4. **Not editable until ready.** A visit takes an edit only when its lock
   is decided writable *and* its sidecars have loaded. Until both it is
   "opening": every edit control is off, and an edit that gets through is
   refused before the cache changes. An edit can never build a record from
   an empty cache. While the lock is undecided, nothing derived is written.
5. **A job result lands only under the app's lock.** A result (Auto Tone,
   scoring, Chromatic Aberration), carried over or not, for a photo whose
   folder is not editable goes the way of a write into a left folder: it
   is queued and lands under the folder's lock, or is **parked** while
   another program holds the folder. It never lands without the lock. If
   the visit ends read-only, it stays parked, and lands if and when the
   app holds the folder (a reopen, or at quit if it is free).
6. **Nothing waits on the UI thread.** All lock state is the UI thread's.
   It tries a lock once, with no wait, at open and then once a frame while
   the visit is opening. The load thread only reads sidecars.
7. **A crash never blocks anyone.** The OS releases the lock.

## 2a. Every write is a field change

A sidecar write never carries a whole record. Each write names the fields
it changes, and is applied to the record read from disk when it runs,
under the folder's lock (native). So a field the cache does not know (a
parked job result, a write from a left visit, another program's write
before the lock) survives every edit.

| Change | Fields |
|---|---|
| Rating, flag, label | that one field |
| Adjustments | only the sliders that differ between before and after, onto what the sidecar holds |
| Spot removals, masks, rotation | that field; masks keep foreign entries |
| Detection | one target's coverage id |
| Score | the stored score |
| Delete | the record (its sidecar goes) |
| Group | the group file, whole: a group file is the app's alone |

* The cache is changed from the same change, never the other way round.
* An edit (adjustments, spot removals, masks, detection, rotation) moves
  the edit time when it changes the edits, on disk as in the cache.
* Writes for one photo keep their order. A load that read the disk before
  one of our writes landed gets every change since its mark applied again,
  in order.
* Undo is a change too: it sets the fields back as any edit does.
* Each change is its own read and write; changes are not coalesced.
* A change that can't apply to a record that read fine (adjustments that
  can't be merged) fails its write with an error. The file is not moved
  aside, and nothing is written whole instead.
* Reading the sidecar can fail three ways:
  * an I/O error that may pass: the write parks and is tried again, and
    the user hears after about 3 s;
  * an I/O error that won't pass (no permission, not a file): that one
    write fails at once, with the reason;
  * a file that read but isn't a record we understand (it doesn't parse,
    or is over the size cap). If the change carries the cache's copy of
    the record, the new record (the change onto that copy) is written to
    a temp file in `.lightphotos`, then the bad file is renamed to
    `<sidecar>.unreadable-<n>` (never deleted), then the temp file is
    renamed into place: a failure at any step leaves the old sidecar or
    the new one, never neither. The user hears once, with the photo's
    name. Without a copy (a result for a photo the cache doesn't hold),
    the file is left as it is and the write fails, naming the photo.
* Parking keeps order per path (one photo's sidecar, one group file). A
  parked or failed write for one photo never holds back writes for other
  photos in the folder.
* Every notice is shown: a few in full, more as the first two and a
  count.
* A job result for a photo the cache holds goes into the cache at once,
  whatever the lock's state; its write parks until the app holds the
  folder.
* The browser applies the same change to the record it reads in the same
  task. It refuses an edit until the folder's sidecars have loaded, as
  native does.

## 3. Folder key

The lock and the "is this the open folder" test use one key per folder.

| Case | Rule |
|---|---|
| Two spellings (`a/b` vs `a/./b`, trailing `/`) | key = `canonicalize(dir)` |
| Relative photo path | make absolute before keying (canonicalize the parent) |
| Symlinked root | canonical path (the target); two links to one folder share one lock |
| Case-insensitive volume (APFS, NTFS) | canonical path from the OS is the stored case; compare keys as the OS returns them |
| Folder gone | no key, no lock; writes into it fail ("folder is gone"), never recreate it |

All lock code takes a `FolderKey` type, built only by `FolderKey::of(dir)`.
No `Path` comparison of folders outside that type.

## 4. Lock lifetime (app)

```
open(A) --try once--> Held(A) --leave--> Draining(A) --queue for A empty--> released
            |                                 ^
            |                                 | reopen(A) takes it back
            +--busy, named holder--> ReadOnly visit (no lock, nothing written)
            +--busy, unnamed / own pid--> Opening: try once a frame <= 100 ms, then ReadOnly
            +--busy, app-quitting--> Opening: try once a frame <= 15 s, then ReadOnly
            +--no lock support--> Lockless visit (writes, notice once)
quit: released after the sidecar writes land.   crash: OS releases.   relaunch: the old process is gone,
so its lock is gone; a holder line naming a dead pid is ignored.
```

| Event | Lock |
|---|---|
| Open folder with photos | the UI thread tries it once, no wait. Busy for a moment: the visit is "opening" and each frame tries once more. The load thread never touches a lock |
| Leave or close while opening | the attempt ends; no lock is taken after. If this visit put the folder in the derived-file writers' read-only set, it leaves it; a folder decided read-only earlier stays |
| After a leave or close | the left visit is neither editable nor held: job results go the left-folder way |
| Open folder without photos | none taken; nothing written |
| Leave / close | kept until every queued write for the folder has landed |
| Write into a folder the app has left | the writer thread takes the folder's lock once (no wait). It lets go as soon as the next write is for another folder, or the queue is empty. If another program holds it, the write is **parked and tried again** about every 0.5 s, and at quit (not dropped: rule 1). Later writes for that photo wait behind it, so its order stays; other photos' writes go on. A write done inline (no writer thread) parks the same way. Parked writes live in state shared with the writer, so they outlive a writer thread that dies; `pump` and the quit flush notice a writer that has ended and try them inline every 0.5 s. A write that panics fails with an error the user sees, alone. A lock that can't be made just now (`EIO`, too many open files, a network blip) parks the write like a busy one; after about 3 s of tries the user is told, and it keeps trying. A lock that can never be made (a read-only volume, no permission, the folder or `.lightphotos` gone or not a folder, a planted link) fails the write at once, with the reason. The quit flush reports writes that are only parked and returns, rather than wait out its timeout. Writes still in a dead writer's channel are drained and done inline |
| Job result for a folder not editable (opening, or read-only) | the same path: queued, landed under the lock, or parked (rule 5) |
| Quit | the app first marks its holder lines `app-quitting`; queued writes flush; then it lets go of every folder whose writes have all landed, before the catalog and signal flushes. A folder with a write left keeps its lock until the process ends |
| Relaunch while the old process still exits | a holder line `app-quitting`: the visit is opening, and each frame tries again while that holder lives, up to about 15 s |

One owner object holds each `FolderLock`. The UI thread and the writer
thread hand locks over through it; no thread holds a mutex across I/O,
and a lock our process holds is never "busy" to our own process.

## 5. Holder line

* Lock file: `.lightphotos/.lock` (OS lock, never deleted).
* Holder line: `.lightphotos/.lock.who` = `pid mode` (`app`, `app-save`,
  `app-quitting`, `mcp`). No lock covers it, so Windows can read it.
* Written right after the lock is taken; deleted before release.
* A busy lock whose line names our own pid is ours (hand-over in
  progress) → retry. A line naming a dead pid (crash) cannot occur with a
  held lock; it is ignored.

## 6. Read-only visit

| Never written | Never deleted |
|---|---|
| sidecars, groups | coverage files (even if they read as corrupt) |
| coverage files (detection runs in memory) | thumbnails, orphan sweeps |
| thumbnail cache entries | `.lock.who` of the holder |
| signal cache (kept in memory) | signal cache entries (no orphan sweep) |

Job results carried over into the folder are parked, not dropped (rule 5).
A delete the user starts is refused. One a job finishes there (a bulk
delete started while the folder was writable) is parked like any result.

Read-only applies to the photos of that folder only (by `FolderKey`). The
coverage store and the thumbnail cache keep a set of every folder opened
read-only this session (and of the folder while it is opening), so a job still running for one after the user
moved on writes nothing into it. Opening the folder writable takes it out
of the set. A job carried over from another folder keeps that folder's
rules.

Shown as a line under the folder name (Grid, Loupe, Develop), with the
holder (another window / `lightphotos-mcp`, pid). `get_state` (app and
headless MCP) reports `read_only` with the reason. Export still works;
masks bake from memory.

**When a brief holder can cause it:** the open tries again, once a frame
for about 100 ms, only when the busy lock names no holder (a `get_state`
probe) or names our own process (a hand-over). A holder another process
names is not waited for, except `app-quitting` (while it lives, up to
about 15 s). A visit decided only at the first try sweeps derived files;
an opening one does not. So a headless MCP
command that is writing at the moment of open makes the visit read-only.
Reopen the folder to try again (there is no "Try again" button).

## 7. Headless MCP

* Takes the lock for each writing command (read → write), waits up to 2 s
  for another brief holder.
* If the app holds it: fails at once, names the app's pid, says `--attach`.
* Reads (`get_state`, `render`) never take the lock; `get_state` probes
  without writing.

## 8. Volumes

| Volume | Behaviour |
|---|---|
| Local (APFS, ext4, NTFS) | full design |
| No lock support (`ENOTSUP`, `ENOLCK`, some SMB/NFS) | Lockless visit: app writes, one notice "don't run headless MCP here" |
| Lock file this user can't write (another user's `.lock`, mode 0444) | opened read-only; the OS lock works on a read-only handle, so the visit is normal |
| `EINVAL` from the lock call (the file system can't lock this file) | Lockless visit, like `ENOLCK` |
| `EIO` or another error (a disk or network fault, may pass) | ReadOnly visit, reason "can't be locked"; reopen to try again. A write that meets it later parks and retries |
| Browser, no access to the folder (no directory handle) | ReadOnly visit, reason "no access to this folder"; open it again |
| Read-only volume / no permission for the folder | ReadOnly visit, reason "can't be locked" |
| Windows | `LockFileEx`; holder line in the separate file. Verified only on CI |
| Browser | no locks; unchanged |

## 9. Failure cases from the review rounds

| # | Case | Handled by |
|---|---|---|
| 1 | App's stale cache overwrites MCP's field | MCP can't write while the app holds the folder (rule 2) |
| 2 | MCP's read-then-write overwrites the app's field | same |
| 3 | Merge bookkeeping bugs (overlay, base, undo of merged sliders) | no merge at all |
| 4 | Lock released while the folder's writes are queued | Draining state (§4) |
| 5 | Writer thread finds our own UI lock busy (race) | one owner object; own pid never busy (§4, §5) |
| 6 | UI open finds our writer's lock busy → writable folder opens read-only | hand-over, own-pid retry |
| 7 | Probe by `get_state` makes the visit read-only | probe never takes the lock exclusively for long; open retries unnamed busy |
| 8 | Two path spellings of one folder open read-only | `FolderKey` (§3) |
| 9 | Relative photo path misses the open-folder test | `FolderKey` (§3) |
| 10 | Quick relaunch opens read-only (old process still exiting) | the quitting app marks its line `app-quitting`; open tries again for up to 1 s (Q3) |
| 11 | Writer refuses its own writes in a race | rule 1: app writes are queued, never refused |
| 12 | Quit flush shorter than a lock wait | no waits in the app (rule 6) |
| 13 | Waits on the UI thread / writer thread | rule 6 |
| 14 | `create_dir_all` recreates a deleted folder | `FolderKey` of a gone folder = none |
| 15 | Lock taken on empty / container folders, SD cards | only folders with photos |
| 16 | Transient error remembered as "lockless" | only `ENOTSUP`/`ENOLCK`/`EINVAL` remembered |
| 17 | Unavailable lock at open gives editable visit with unsaved edits | ReadOnly "can't be locked" |
| 18 | Read-only edits shown then reverted | controls off; gates at key / command / UI action |
| 19 | Undo, mask Delete, tool keys bypass gates | gate before tool keys; `check()` gates undo |
| 20 | Detection writes coverage / deletes corrupt file in read-only | §6 |
| 21 | Export drops Sky/Subject mask in read-only | in-memory coverage |
| 22 | Sweeps delete the other writer's new files | no sweeps in read-only |
| 23 | Holder line unreadable on Windows; stale line shows our own pid | separate `.lock.who`, cleared before release |
| 24 | Job result lost when the drag it landed in is undone (save_patch) | 3.6: a result is a change applied onto the saved edit, and kept through undo |
| 25 | Thumbnail cache entries written in a read-only visit | the thumbnail cache skips writes for the read-only folder |
| 26 | A `get_state` probe at the moment of open makes the visit read-only | open retries an unnamed busy lock (§6) |
| 27 | A left folder's lock held until the whole queue empties | the writer lets go when the next write is for another folder |
| 28 | A carried-over scoring run uses the new folder's read-only mode | read-only is per folder (`FolderKey`) in the coverage store |
| 29 | Quit while a left-folder write waits for another program | parked writes are tried again at quit; what is left is reported |
| 30 | A second app instance on one folder | opens read-only (Q4) |
| 31 | Signal cache written and swept in a read-only visit | §6: kept in memory |
| 32 | The open sleeps on the UI thread for the lock | rule 6: one try a frame, never a sleep |
| 33 | An inline write (no writer thread) into a held folder goes ahead unlocked | it parks like a queued one |
| 34 | A lock file this user can't write opens read-only | the file is opened read-only (§8) |
| 35 | The quitting app holds the lock through the catalog and signal flushes; a relaunch gives up after 1 s | released after the sidecar writes; relaunch waits up to about 15 s |
| 36 | A folder opened read-only becomes writable for derived files once the user moves on | a set of read-only folders (§6) |
| 37 | A writer thread that dies loses its parked writes | parked writes in shared state (§4) |
| 38 | Each frame asks the disk for the key of every folder with a write queued | the key is worked out once, when the write is queued |
| 39 | A stale load thread's lock dropped while the visit is writable | the load thread takes no lock (rule 6) |
| 40 | Thumbnails and coverage written into a folder still opening | opening counts as read-only for derived files (§6) |
| 41 | A job's write while opening lands in a folder that turns out read-only | parked until the app holds the lock (rule 5) |
| 42 | Close while opening leaves a held lock | the attempt just ends (§4) |
| 43 | A folder's kept key is stale after a volume is mounted again at its path | the key is worked out afresh at each open; no "no key" is kept; a write is uncounted under the key it was counted under |
| 44 | A load thread takes a lock after quit began | the load thread takes no lock (rule 6) |
| 45 | Inline parked writes tried only on the next write or at quit | `pump` tries them every 0.5 s, as the writer thread does |
| 46 | A panic while parked writes are tried again loses all of them | each leaves the queue only once done; a panicking one fails alone with an error |
| 47 | Auto Tone pressed while a folder opens is dropped | opening defers as loading does; only read-only refuses a job |
| 48 | A relaunch tries in the gap where the quitting app has cleared its holder line but not let go, and opens read-only | once an attempt has seen `app-quitting`, it keeps that wait |
| 49 | A sidecar, group, coverage, thumbnail or signal write recreates a photo folder deleted or unplugged while the app works in it | `.lightphotos` is made only inside a photo folder that exists (`paths::create_sidecar_dir`); the lock makes it with `create_dir`, not `create_dir_all` |
| 50 | An edit after the lock is held but before the sidecars load replaces the photo's saved sidecar with a default record | not editable until both (rule 4); the store refuses it before the cache changes |
| 51 | A carried-over result for a folder another program holds lands without the lock | `patch_uncached` and every job result go through the writeback, parked until the app holds the lock (rule 5) |
| 52 | Held-back writes are put back with stale records, or refused while the user's change shows | no hold-back at all; edits are refused up front (rule 4), results park (rule 5) |
| 53 | A parked write that panics stays first and ends every new writer | it fails alone with an error (§4) |
| 54 | After the writer dies, nothing retries the parked writes | `pump` and the quit flush notice it and retry inline (§4) |
| 55 | A folder left while opening stays read-only for derived files | it leaves the set; only a folder decided read-only stays (§4) |
| 56 | The UI blocks on a hung network mount at open | known limit (§11) |
| 57 | An edit is a whole record built from the cache, so a stale cache (a result that went to the sidecar only, a carried-over result after a leave, a write behind a parked one) overwrites newer fields on disk | every write is a field change applied to the disk record (§2a) |
| 58 | On the browser an edit before the load lands replaces the sidecar | the store refuses it on every target until the load lands |
| 59 | A detection while the folder isn't editable is dropped | a detection is a field change that parks like any result |
| 60 | Leaving undecided drops a read-only decision made earlier in the session | only what this visit added is taken out |
| 61 | After a close the left visit is still editable and held | a left visit is neither |
| 62 | A lock that can't be made just now fails an accepted write for good | parked and tried again; the user is told if it goes on |
| 63 | A dead writer's channel loses the writes still in it | they are drained and done inline |
| 64 | A user delete reaching a read-only open folder lands later | refused before anything is queued |
| 65 | The event loop wakes every 16 ms for the whole sidecar load | 16 ms only while the lock is undecided; the load keeps its own wake |
| 66 | An unreadable, unparseable or oversized sidecar fails every later edit for good | passing I/O errors park, lasting ones fail that write; a file that isn't a record is replaced from the cache's copy, the bad file kept aside (§2a) |
| 67 | A lock that can never be made parks writes for ever, and the quit flush waits out its timeout | such writes fail at once; the flush reports parked writes and returns |
| 68 | In the browser, a folder with no handle opens for ever | a read-only visit, "no access" |
| 69 | A bulk delete that finishes after the folder reopened read-only drops its sidecar deletes | job deletes park like other results |
| 70 | A job result after the load but before the lock is decided reaches the disk but not the cache | it goes into the cache too |
| 71 | Adjustments that can't be merged are written whole | the write fails with an error |
| 72 | The write history grows, and trimming it scans it on every write | it keeps only what a load in flight can replay, trimmed from the front |
| 73 | Coalescing reads ahead of the queue and groups writes, with its own bugs | removed: each change is one read and one write |
| 74 | A good record is moved aside when a change can't apply to it; a bad one replaced with an empty record when the change has no cache copy; a crash between steps leaves no sidecar | move aside only a file that isn't a record, only with a cache copy, in the order temp file, move aside, rename (§2a) |
| 75 | A read error that won't pass parks for ever; a parked write for one photo holds back the whole folder | lasting read errors fail that write; parking keeps order per path |
| 76 | A read-only visit's forget writes whole group files from its cache | it skips the group half; the next writable load reconciles the groups |
| 77 | The quit flush blocks on the parked queue while the writer holds it | it only tries the lock, so the deadline holds |
| 78 | Only the last notice is shown | all, or the first two and a count |

## 10. Decisions

| Question | Decision |
|---|---|
| Q1 A write into a left folder that another program holds | stays queued and is tried again; never dropped |
| Q2 Leaving read-only | reopen the folder; no "Try again" button |
| Q3 Quick relaunch | try again, off the UI thread, while the holder line says `app-quitting` and that holder lives (up to about 15 s) |
| Q4 Second app instance | opens the folder read-only |
| Q5 Lockless volumes | a notice once; no extra MCP refusal |
| Q6 Symlinked roots | one lock per resolved folder (`FolderKey`) |

## 11. Known limits

* The UI thread resolves the folder's key (`canonicalize`) and tries its
  lock (`flock`) at open, and once a frame while it is opening. On a
  network mount that hangs these block the UI. The folder listing already
  touches the volume on the UI thread at open, so this adds no new kind
  of wait.
* The browser build has no locks and keeps its old rules.
