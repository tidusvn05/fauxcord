# bee-twilight

Checks the Discord surface a twilight 0.16 bot (bee-harness's Discord runtime) relies on, with
that bot's exact twilight features and gateway intents: READY, mentions, replies, threads, DMs,
attachments, slash commands, button presses, and RESUME with replay.

```sh
PORT=3999 pnpm start                # one shell
FAUXCORD_PORT=3999 cargo run        # another: PASS/FAIL per step, exit 1 on any failure
```

It needs no Docker and is not part of the `compat/` CI matrix; run it after syncing upstream.
