# Blog idea backlog

Subjects mined from this repository that have not been written yet. Posts are published on lightphotos.app, one Astro page each under `src/pages/blogs/` in the site repo, listed on `/blogs.html`. Six of these subjects have already gone out that way: how a photo gets on screen, what ImageIO gives you free, where your data lives, never load the whole folder, measuring a GUI app, and six queues and a reserved worker.

Each entry below gives the takeaway first, then enough of the story and the evidence to write from. Everything cited here was checked against the tree, so a draft can start from the citation rather than from a search.

The house style is set by the existing posts: first person, open on a real situation, explain photo-specific words the first time they appear, no invented numbers, and close on a "My learnings" list.

## Deep dives

**1. Let the computer find the number instead of deriving it.**
Auto Tone does not compute slider values from a formula. It picks a value, runs the real tone pipeline, compares the result to a target, and halves the interval twenty-four times. Bisection costs nothing next to decoding a photo. The payoff is not speed, it is that the analysis calls the same function the renderer calls, so it cannot drift from what you see on screen. The idea generalises to any case where you own the forward function and need an input that produces a wanted output: pricing, quota solvers, layout constraints, picking a retry budget that hits a target success rate. Numerical inversion turns a correctness problem into a convergence problem, and convergence is much easier to test.
Evidence: `src/autotone.rs:203-223`, `src/autotone.rs:41-42`, `src/develop.rs:410`.

**2. When you bucket a population, keep a real member of each bucket.**
The histogram compresses a photo into 256 brightness buckets, and each bucket keeps one actual colour rather than a grey of that brightness. The comment gives the counterexample: pure red displays at brightness 0.299, while a grey carrying red's underlying light level displays at about 0.578. A grey stand-in biases every percentile in the same direction, so the error is systematic rather than noisy. The same trap appears when you average latencies per service to estimate a p99, or model "a typical user" as the mean of a feature vector.
Evidence: `src/autotone.rs:47-63`, `fit_to_level` at `:91-113`, tests at `:458`, `:482`, `:504`.

**3. A sorted order stops being sorted once you transform the values.**
Percentiles over the buckets are only valid if the transform preserves the ordering the buckets were sorted by. Photo adjustments do not: a red patch can start brighter than a grey patch and end darker. So the buckets are re-sorted by output brightness for every candidate adjustment. Two tests construct exactly that reversal. Applies to any cached ranking that gets rescored, and to reusing a sort index across a transform.
Evidence: `src/autotone.rs:164-185`, tests at `:458` and `:482`.

**4. Store the value in units the thing that keeps changing cannot touch.**
The viewer shows a photo through up to four decode qualities, and the photo's true dimensions are not known until metadata arrives. Zoom was stored as an absolute ratio, so the moment the true size landed, a 24-megapixel photo snapped about 2.3 times smaller and slid toward the top-left corner. The fix was not to compensate at the five places where the size can change. It was to store zoom as a multiple of fit-to-window, which makes the transform independent of the photo's dimensions entirely. Same move as storing relative timestamps rather than absolute ones, or percentages rather than pixels.
Evidence: `plans/loupe-zoom-dimension-invariance.md`, commit `2bc20eb`, `src/app/loupe.rs:36-46`.

**5. One counter cannot mean two things.**
Browser file handles are keyed by the picked folder's name, so re-picking a folder with the same name would let old work resolve against the wrong handle. Jobs therefore carry a generation number and stale results are discarded on arrival, because a Web Worker cannot be interrupted mid-decode. The first version bumped that counter on every folder-tree action, including collapsing a folder that changes nothing, so holding an arrow key threw away every decode in flight. Split into one counter for the handle map and one for directory listings. The tell that you have the wrong counter: it bumps on an action that changes no state the job reads.
Evidence: commit `fa88b12`, `src/app/web.rs:266`, `src/app/mod.rs:470-476`.

**6. A cache key is an agreement between two programs.**
The thumbnail cache key is deliberately worse than it could be. It rounds the file's modification time to milliseconds, because that is all a browser reports, so the native and browser builds compute the same key for the same untouched file. It leaves the path out, so moving a folder keeps its cache. It carries a version string, so bumping one character invalidates every cached thumbnail everywhere. And the key is in the entry's filename, which turns a stale entry into an orphan a directory listing can find, rather than a wrong answer. The hash is a hand-rolled FNV-1a, because the language's default hasher is randomised per process and cannot key anything stored on disk.
Evidence: `src/thumbnail.rs:329-341`, test at `:610`, `src/hash.rs:3-4`, commit `ace2004`.

**7. If two pieces of code must agree, make one call the other.**
One tone pipeline exists in three forms: the saved settings, a packed struct uploaded to the GPU, and a CPU implementation. The live histogram and the auto-tone analysis both call the CPU implementation rather than reimplementing it, so that whole class of drift is impossible. The GPU shader is the one copy that cannot call anything, so it is kept deliberately simple and marked on both sides. The rule: rank duplicated computations by whether the duplication is forced, collapse the ones that are not, and minimise the surface of the ones that are.
Evidence: `src/develop.rs:409`, `src/shader.wgsl:97`, `src/app/histogram.rs:56-64`.

**8. One code path is the whole promise.**
Exactly one function turns edits into finished pixels, and it has two callers, the exporter and the thumbnail you are looking at. That is the entire reason an exported JPEG matches what was on screen. Stated as a call-graph property, the guarantee becomes something a reviewer can grep for. The layering detail is the better half: baking happens at upload time rather than being cached, because the thumbnail cache key is size and modification time, so editing a slider re-runs only the cheap step and not the expensive decode.
Evidence: `src/image_ops.rs:3-5`, `src/export.rs:53`, `src/app/thumbs.rs:695`.

**9. Save what the user asked for, not what you computed.**
Exposure was rewritten from a plain multiply to a film-like curve, with no data migration, because the saved value records an intent ("+2 stops") rather than a result. Design persistence around intent and you keep the right to improve the algorithm forever. Second lesson in the same change: a slider was found running backwards, and it was found by feeding in 0.95 and printing 0.636, not by reading the code. For numeric pipelines, reasoning about the sign is not a diagnosis.
Evidence: commit `a98762d`.

**10. Copying values into a cache is not syncing.**
The in-memory mirror of ratings and edits was refreshed by a loop that only inserted. A rating cleared by another tool therefore survived in memory, looking perfectly plausible. The deletion half of a sync has no test that fails naturally, because the symptom is a stale value rather than a crash. Whenever you write a merge loop, ask what happens to keys present in the destination and absent from the source. If the answer is "they stay", you wrote a union.
Evidence: commit `196e805`.

**11. Tests that read your own source code.**
Three layers guard translations, each catching what the one above cannot. The type system catches missing strings, because each language is a struct and a missing field will not compile. A test scans the interface source for English literals passed into a list of widget calls, and that scanner has its own unit test with a fixture. A third test opens the bundled Chinese font and checks it contains every character the translation file uses, reading that file as text at compile time, and the script that generates the font subset uses the same character rule the test does. The last layer is the one people skip, and a subsetted font fails by silently drawing boxes.
Evidence: `src/i18n.rs:957-1000`, commit `a0fe6d0`, `scripts/subset-cjk-font.sh`.

**12. Retire the old store only after a pass that skipped nothing.**
Migrating from a single global catalog to per-folder files had to cope with folders on unmounted drives. Each row was fanned out to its target directory, skips were counted, and the legacy database was retired only when a full pass skipped zero rows. Every step was safe to repeat, and both "already migrated" and "nothing to migrate" counted as success, or the migration would never converge. The right shape for backfills and cutovers generally.
Evidence: historical, `git show 016f0af:src/catalog.rs`.

## One-line seeds

Too thin to carry a post alone. Good as sections inside a larger one.

- Two quality levels of the same cached item need one shared memory budget, or "eight previews" quietly becomes sixteen resident images. Commit `ebaa369`.
- When two files must agree on an order, put the order in a third place both read. A table carrying a field accessor as data drives both the panel layout and the keyboard shortcuts. Commit `df52f4d`.
- Cache-bust a file your build tool refuses to fingerprint by reading the fingerprint of one it does out of the rendered page. Commit `3d409df`.
- Making a function asynchronous usually splits it into more than two pieces, one per way the work can resume. `plans/web-subfolder-browsing.md`.
- Compile the other platform's decode path on yours behind a feature flag, so its tests run in your normal test suite. Cfg-gating by operating system alone means the code you cannot run is also the code you cannot test. `Cargo.toml`, the `raw-probe` feature.
- Detecting that a file is gone is the easy half of cache invalidation. Detecting that it changed is the half you skip, because it costs a read per file. Commit `b528f8d`.
- A background cleanup task must draw from the same concurrency budget as the foreground, or your carefully tuned limit is now the limit plus one. `src/app/web.rs:8-16`.
- Retry delays without randomness make everything that failed together fail together again. The same file uses a delay drawn uniformly from zero to a doubling cap.
- Handing a buffer to a worker in a browser moves it rather than copying it, so a second consumer needs an explicit clone, and forgetting that fails at runtime in the other language. `src/web/web_fs.rs:139-152`.
- Without stack unwinding, every dependency's internal assertion becomes a process kill, so you end up writing a predicate that mirrors the library's private preconditions. `src/raw/preview.rs:102-113`.
- Mixing scripts in one line of text mixes typographic conventions, and fallback fonts do not share a baseline. Read the metrics with the same library your renderer lays out with. Commit `bd7cc1f`.
- You can add a method to a framework's Objective-C class at runtime, at the price of three couplings the compiler cannot see: the class name, the selector's type encoding, and the ordering. Degrade rather than assert. `src/macos_delegate.rs:67-85`.
- A knowingly imperfect fallback is fine when the comment bounds how often it runs. Commit `352dac8`.
- Reap leftover temp files past the longest a real write can take, rather than never. "Might be in use" is a reason to find the bound, not a reason to leak. Commit `3d07e51`.
- A quality threshold is policy and belongs with the caller who pays for it, not inside the shared function. Two callers with opposite cost functions is the tell. Commit `8b6f2da`.
- Two platform implementations grow two vocabularies for one idea, and renaming them is the cheapest architecture work available. Commit `13d3f76`.
- A feasibility memo written as a prediction rather than a conclusion, and gradeable afterwards against what shipped. Its threading prediction was wrong and its memory prediction was right. `plans/web-wasm-port-feasibility.md`.
- Before optimising an expensive recomputation, check whether it is being triggered by its own inputs. Duplicate grouping was rerunning on arrivals that could not change its result, which was a thirty-fold win for a much smaller change than making it incremental. Commits `85e0ee0`, `c82e323`.
- A platform machine-learning API will report a confident subject in a photo that has no subject, and no coverage statistic catches it. `PROGRESS.md`, the 2026-08-12 entry for plan-d2 task 3.
