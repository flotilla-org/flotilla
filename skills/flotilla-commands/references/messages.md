# Messages

Resolve the recipient through the current contacts and charter before sending an
authorized message. Use the exact address returned, or a local role when it is
unambiguous. Prefix a role with `@` if its name is also a crew command.

```sh
flotilla message contacts
flotilla crew RECIPIENT handoff --message BRIEF
flotilla crew @list handoff --message BRIEF
```

Store substantial context as a brief artifact, then put its reference in the
handoff message. Pass an absolute file path:

```sh
flotilla artifact put --kind brief --about SUBJECT /tmp/handoff.md
```

Use `--carry` on handoff when typed resource/revision references are needed;
obtain the JSON shape from the actual Message reference rather than inventing
an address encoding. Read the command's help for the current argument shape:

```sh
flotilla crew RECIPIENT handoff --help
```

## Delivery rationale

Handoff ensures the target is running and delivers a message. Read contacts
again after a charter change so the target and supervision chain stay current.
The daemon reads artifact contents from the supplied file. Use artifact files
for durable context that should survive a terminal turn.
