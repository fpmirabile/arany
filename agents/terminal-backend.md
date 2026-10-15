# Terminal backend and cursor ownership

Load for the private Ratatui backend or cursor-response decoding. [Terminal rules](./terminal.md), [history rules](./history.md) and [security rules](./security.md) still govern its callers.

- Ratatui Inline requires cursor observation at acquisition and resize. Its ordinary Crossterm Unix query writes to stdout and opens a competing input path; Arany routes the query through its stderr backend and the sole admitted raw reader instead.
- A query has one pending response, a one-second deadline and closed bounded coordinates. Unsolicited responses are discarded. Query bytes never enter drafts, stdout or canonical history. Input backpressure remains bounded and shutdown joins the reader.
- Visual geometry has at most 4,096 columns and 262,144 cells. Zero or excessive geometry fails typed ownership before allocating Ratatui buffers; linear output has no visual buffer.
- Classic clearing starts at its owned viewport, including Ratatui horizontal-shrink clearing. It never clears settled primary-screen output. Cleanup uses the stored owned area and cannot query a stopped reader.
- Fullscreen and temporary detail ownership restore the alternate screen and mouse modes through the same RAII owner. Detach backend output after real cleanup before Ratatui destruction.
- Ratatui scrolling-regions insert_before needs at least two free rows above the viewport; insert_native handles the full-screen and one-free-row cases itself.
- Native console APIs may supply cursor coordinates without a query. An unavailable or mismatched native output capability must choose linear before work or fail typed ownership; no guessed coordinates or second input consumer.
- TestBackend proves layout; native PTYs with an explicit cursor peer prove acquisition, channels, key retention, timeout and cleanup. Emulator scrollback/resize behavior requires its own real-terminal or tmux evidence.
