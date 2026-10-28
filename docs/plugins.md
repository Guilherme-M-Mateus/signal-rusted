# Plugins

A plugin is any executable program (script or binary) in the `plugins/` folder of the data
directory (Settings → Plugins → *Open plugins folder*). Plugins are off until you enable them
there. Each enabled plugin runs as a child process, started when signal-rusted connects and
stopped when you disable it or quit.

**A plugin reads your messages and can send messages as you. Only run plugins you trust.**

## Protocol

JSON, one object per line. Events arrive on the plugin's **stdin**; the plugin writes actions
to its **stdout**. Anything else (logs) goes to **stderr**. Lines on stdout that aren't valid
actions are ignored and logged.

### Events (app → plugin)

A message arrived, or one you sent from another device was synced (`outgoing: true`):

```json
{"type":"message","chat":"<chat key>","chat_name":"Alice","from":"<uuid>","from_name":"Alice","outgoing":false,"timestamp":1730000000000,"body":"hi"}
```

You sent a message from signal-rusted:

```json
{"type":"sent","chat":"<chat key>","chat_name":"Alice","timestamp":1730000000000,"body":"hello"}
```

Only messages with text produce events. Messages a plugin sends don't produce events, so a
plugin can't trigger itself.

### Actions (plugin → app)

Send a text message to a chat in your chat list (not to unanswered message requests):

```json
{"type":"send","chat":"<chat key from an event>","body":"text"}
```

Show a desktop notification:

```json
{"type":"notify","title":"Reminder","body":"Stand up"}
```

## Example

[`examples/plugins/auto-reply.py`](../examples/plugins/auto-reply.py) answers "!ping" with
"pong". Copy it into the plugins folder, make it executable (`chmod +x`), and enable it.
