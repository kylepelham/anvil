# Kyle's style

Scope: 

- Scope names files or folders: restyle that code, behaviour unchanged.
- Scope names a task: do the task, writing every kept file in this style.
- Scope is empty: restyle the code you wrote or changed in this session, and write kept code this way for the rest of the session.

The reference sample is Kyle's hand-written obs-c (2024). The style is **airy code**: short paragraphs separated by blank lines, a short step comment over a block where it helps, full-word names and a predictable layout. Readability beats line count every time.

A restyle is an **improvement pass, not a rewrite**: every change must leave its spot more readable than before. Code that is already readable stays exactly as it is (see Preserve).

## Steps

1. **Sort kept from throwaway.** Kept code is anything that lands in the repository tree or that Kyle will read again. Throwaway code is one-off scripts, probes, debug harnesses, migrations run once, and anything in the scratch directory or due for deletion after it runs. Write throwaway code as compact and token-cheap as the language allows: no comments, no paragraphs, short names, one-liners. Existing one-shot scripts in the repo (an `oneshot/` folder, drivers and probes cited as evidence in logs or docs) are out of scope for a restyle: leave them untouched. The rest of this document is for kept code only.
2. **Read before restyling.** Open the files in scope and their callers. When a file already has this style, match it exactly. When the repo has a formatter config, check whether the existing code actually follows it: obs-c ships a stock cmake-init `.clang-format` (2 spaces, 80 columns) that Kyle's code ignores. Ignore unfollowed configs; when a formatter is enforced (CI, pre-commit), let it own the mechanics, since paragraphs and comments survive formatting anyway.
3. **Apply the reference below**, rule by rule, to every kept file in scope, with Preserve taking priority over every other rule.
4. **Keep behaviour identical.** Rename locals, private members and file-internal helpers freely. For names other code depends on (public headers, exports, cross-module calls), list the proposed renames and ask before changing them. Before changing any line, grep the repo for tools that match source text exactly (mutation harnesses, tests that patch or search source by string); a line such a tool names stays byte-for-byte as it is.
5. **Verify.** Build and run the tests the repo has. Then read your own diff hunk by hunk and revert any hunk that breaks a Preserve rule or does not read better than the original. Done means every kept file in scope passes the checklist at the end and the build is green; if you could not build, say so.

## Preserve

These are already good; keep them through every edit:

- **Aligned columns.** A run of `#define`s, struct fields, enum values, `case` returns, a field-by-field fill (`rec.magic         = ...`) or argument comments whose trailing comments or values share a column stays aligned. When a rename changes a name's width, re-pad the whole group so the column still lines up. Example that must survive as is:
  ```c
  #define EM_NOP   1u     /* 90, 66 90, 0F 1F /0                              */
  #define EM_PUSH  2u     /* push r64                                         */
  ```
- **Comment wording and wrap.** Leave an existing comment as written unless it is wrong, and then fix only the wrong words in place. Keep the file's wrap width: never reflow a comment block to a new width (that leaves orphan words on their own line). Keep each comment directly above or beside the line it describes.
- **Facts and references.** Every cross-reference (`C36`, `Boot C13`, `review #7`, `finding 4`, doc section names), quoted spec text (verbatim, inside its quotes), invariant, reason, unit, offset and caveat in a comment stays. Shortening a comment that loses any of these makes it worse.
- **Comment syntax.** Existing comments keep their syntax. Convert nothing between `/* */` and `//`.
- **Established names.** Domain short names a codebase uses consistently (`lvl`, `idx`, `vmcb`, `rip`, register names) stay. Expand a short name only when the expansion is clearer and correct for every use in its scope; if one name would cover two different things, keep them distinct.
- **Line width.** Do not join wrapped lines into one past the file's existing longest line.

## Shape

- 4-space indent. Lines may run long (around 150 columns): a `throw` with its formatted message stays on one line.
- **Paragraphs**: group 1 to 5 lines that do one thing, then one blank line. Assignments to the same object form one paragraph. Put a blank line before the final `return` of any function longer than a few lines.
- Blank line after `namespace x {` and before `} // namespace x`. One blank line between functions.
- Braces: out-of-class function definitions get the brace on its own line. Classes, structs, namespaces, control flow and lambdas keep it on the same line. In-class bodies are one-liners (`HANDLE getHandle() const { return handle; }`) or same-line braces.
- `else` and `catch` start a new line after the closing brace:
  ```cpp
  if (!attemptExistingHook()) {
      PRINTLN("Inject new hook. Thread ID: {}", threadId);
      injectGraphicsHook(context.threadId, true, context.is32Bit);
  }
  else
      PRINTLN("Reusing existing hook. Thread ID: {}", threadId);
  ```
- Single-statement bodies: a short guard stays on one line (`if (!hwnd) throw ...;`); otherwise put it unbraced on the next line, indented. Use braces once the body is two or more lines.
- One declaration per line in new code. Ordinary statements use single spaces around `=`; tables (runs of defines, fields, fills, case returns) align their value and trailing-comment columns.
- Calls with many positional arguments (Win32, D3D, C APIs) that overflow a line: one argument per line, `);` on its own line. Bare `NULL`/`0`/`TRUE` arguments may get a short trailing comment naming them, all in one aligned column (from Kyle's `utils.cpp`):
  ```cpp
  if (!CreateProcess(NULL,   // No module name (use command line).
                     ...
                     NULL,    // Process handle not inheritable.
                     NULL,    // Thread handle not inheritable.
                     TRUE,    // Set handle inheritance to TRUE.
                     0,       // No creation flags.
  ```

## Comments

- **Step comments**: a short one-line comment above a block inside a function, saying what the block does: `// Get window handle`, `// Copy the frame data`, `// Cleanup`. Add one where it helps a reader find their way through a longer function. Each must be correct for the block directly under it, and say more than the next line already does (`// Read ModRM` over `modrm = bytes[pos++];` adds nothing).
- A reason goes in where the code would surprise a reader, as one short line: `// Signal the stop event (the hook never signals Exit, so don't wait on it)`.
- New comments are `//` one-liners. Existing comments keep their syntax and wording (Preserve).
- Agent-written comments that narrate history at length may shrink to one line, keeping every fact and reference in Preserve.
- Headers open with Kyle's Doxygen block when the project has one (new projects of his get one):
  ```cpp
  /**
   * @file event.h
   * @author Kyle Pelham (bonezone2001@gmail.com)
   * @brief The event implementation.
   *
   * @copyright Copyright (c) 2024
  */
  ```
  Use the current year for new files. `.cpp` files have no header block.

## Class layout

Data first, then the public API, then a second `private:` for helpers. Access specifiers sit at class indent. Real example:

```cpp
class OBSC_EXPORT Capture {
private:
    CaptureConfig config;
    Context context;

public:
    Capture(const std::string& windowName);

    void attach();
    void shutdown();
    std::tuple<std::vector<uint8_t>, std::pair<size_t, size_t>> captureFrame();

private:
    void initKeepalive();
    void initPipe();
    void initEvents();
    bool attemptExistingHook();

};
```

**Grouping**: inside each section, members sit in groups of one kind, with one blank line between groups, in this order:

1. constructors and destructor
2. static factories (`create`, `open`)
3. their inline variants (`createInline`, `openInline`)
4. operations: actions, then the queries about them (`signal`, `signalled`, `wait`)
5. operator overloads, each non-const and const pair together
6. **accessors last**: `getX()` / `setX(value)`, a field's getter and setter adjacent. An accessor that is one statement is a one-liner in the header; one that needs more (a lock, a conversion) keeps its place in this group but is defined in the `.cpp`. A one-liner always holds exactly one statement.

```cpp
class OBSC_EXPORT Event {
private:
    explicit Event(HANDLE handle) : handle(handle) {}
    HANDLE handle;

public:
    Event() = default;
    ~Event();

    static Event create(const std::optional<std::string>& name = std::nullopt);
    static Event open(const std::string& name);

    void createInline(const std::optional<std::string>& name = std::nullopt);
    void openInline(const std::string& name);

    bool signal() const;
    bool signalled() const;
    void wait() const;

    HANDLE getHandle() const { return handle; }
};
```

```cpp
    T* operator->() { return fileMapping; }
    const T* operator->() const { return fileMapping; }

    T& operator*() { return *fileMapping; }
    const T& operator*() const { return *fileMapping; }
```

The `.cpp` defines members in the header's order; a static factory goes directly after the inline variant it wraps. The same grouping runs everywhere: struct fields by role, constants by `// Category`, includes by origin, teardown mirroring setup.

- **Config and context**: a stateful class holds `config` (an `XConfig` struct of user settings) and `context` (a `Context` struct of runtime state and handles, grouped by role with blank lines between groups, each member default-initialised: `HWND hwnd = nullptr;`).
- **Lifecycle verbs**: the constructor only stores config. Public `attach()`/`init()` does the work by calling private `initX()` steps in order, so it reads as an outline. `shutdown()` tears down in the same groups. Each `initX()` opens with an "already exists" guard that throws.
- **Handle wrappers** (RAII over OS handles): private constructor taking the raw handle, `static create(...)`/`static open(...)` factories returning by value or `std::unique_ptr`, optional `createInline`/`openInline` mutating variants, and a destructor that closes the handle if it is set.
- Helpers are private member functions declared in that trailing `private:` section. Use lambdas for thread bodies and callbacks.

## Naming

- Types `PascalCase`: `Capture`, `NamedPipe`, `FileMapping<T>`, `CaptureType`. Enum class values `PascalCase`: `Memory`, `Texture`.
- Functions `camelCase`, verb first, full words: `attach`, `captureFrame`, `initHookInfo`, `attemptExistingHook`, `waitForConnection`, `runProcessAndCaptureOutput`. Predicates read as questions or states: `is32BitProcess`, `signalled`, `ioPending`.
- Variables and members `camelCase` with full, plain names and no prefix or suffix: `handle`, `threadId`, `hookInfo`, `frameData`, `byteSize`, `threadRunning`. Constructor init lists shadow: `: handle(handle)`. Short names only for established idioms (`hr`, `hwnd`, `sa`, `si`, `pi`) and loop indices.
- Constants and macros `SCREAMING_SNAKE_CASE`, gathered in a `constants.h` under `// Category` comments when shared: `EVENT_CAPTURE_RESTART`, `FILE_HOOK_64_NAME`, `IPC_PIPE_BUFFER_SIZE`, `PRINTLN`.
- Files `snake_case.h`/`.cpp`; public headers in `include/<lib>/`, includes written `"lib/file.h"`. Namespaces are the short lowercase library name.

## Errors, logging, idioms

- Setup failures throw `std::runtime_error` with a formatted message: `"Failed to create the keepalive mutex: ({})"` plus `GetLastError()` or `0x{:x}` of the HRESULT. Queries return `bool` (`signal()`, `signalled()`). Catch only specific types (`const std::exception&`), and log or handle what you catch.
- Log lifecycle milestones through the project's debug macro (`PRINTLN("Created the event {} = 0x{:x}", ...)`).
- `auto` when the right-hand side names the type; explicit types for sizes, handles and HRESULTs. Use structured bindings for pair and tuple results: `auto [device, deviceContext] = createDevice();`. Return braced: `return {std::move(frameData), dimensions};`.
- Simple numeric casts are C-style (`(size_t)mappedSurface.Pitch`); handle and pointer casts use `reinterpret_cast`.
- `#pragma once`; standard and system includes in one group, project includes in another.

## Restyling agent code

Convert each dense habit to its airy form:

- wall-to-wall statements → paragraphs separated by blank lines
- local lambdas doing the function's own work (`poll`, `succeed`, `key`) → private member functions or plain inline steps
- anonymous-namespace helpers taking `Context&` → private members of the class
- `kPollMs`, `state_`, `m_x` → `HOOK_READY_POLL_MS`, `state`, `x`
- one-letter names (`h`, `v`, `n`, `p`) → full names
- `a = x, b = y;`, several declarations on one line, `(cond ? a : b).push(...)` → one statement per line, plain `if`
- `} else {` → `}` then `else`
- `catch (...) {}` → a specific catch that logs or rethrows
- paragraph comments narrating history or ADRs → one short line keeping every fact and reference

Before (agent):

```cpp
bool Capture::waitReady() {
    // The hook fires Ready only when its present hook sees !active + Restart + keepalive alive,
    // so poll rather than block -- a stuck hook would otherwise hang us forever.
    auto poll = [&](DWORD ms) { for (DWORD t = 0; t < ms; t += kPollMs) { if (ctx_.ready->signalled()) return true; Sleep(kPollMs); } return ctx_.ready->signalled(); };
    ctx_.init->signal();
    if (ctx_.restart) ctx_.restart->signal();
    return poll(800);
}
```

After (Kyle):

```cpp
bool Capture::waitHookReady(DWORD timeoutMs)
{
    // Wake the hook up
    context.hookInit->signal();
    if (context.hookRestart) context.hookRestart->signal();

    // Poll for ready (a stuck hook would hang a blocking wait)
    for (DWORD waited = 0; waited < timeoutMs; waited += HOOK_READY_POLL_MS) {
        if (context.hookReady->signalled())
            return true;

        Sleep(HOOK_READY_POLL_MS);
    }

    return context.hookReady->signalled();
}
```

## Other languages

The sample is C++; carry over the shape, step comments, Preserve, class layout order (data, public, private helpers), lifecycle verbs and full-word naming. Take identifier case and brace placement from the language's convention and formatter: C keeps `snake_case` functions and variables with the project's existing prefixes (`sf_`), full words still (`key_path`, not `keyp`); Rust keeps `snake_case` functions and rustfmt braces, with fields first, then an `impl` whose `pub fn`s come before private `fn`s; TypeScript keeps `camelCase`, with fields, then public methods, then private methods.

## Checklist (every kept file in scope)

- Every Preserve rule holds: aligned columns still line up, no comment reflowed or converted, no fact or reference lost, established names and source-matched lines untouched, one-shot scripts untouched.
- Functions are paragraphs separated by blank lines; every new comment is a correct one-line `//` over the block it describes.
- Class order is data, `public:` API, `private:` helpers; stateful classes use config and context; handle wrappers use private constructors with static factories.
- Members are grouped by kind in the Grouping order, accessors last with each getter beside its setter, every one-liner a single statement, and the `.cpp` follows the header's order.
- Names are full words: `camelCase` functions and variables, `PascalCase` types, `SCREAMING_SNAKE_CASE` constants, no member decorations.
- Braces, `else`/`catch` placement and argument-per-line calls match the Shape rules.
- Behaviour unchanged, public renames asked about, build and tests green.
