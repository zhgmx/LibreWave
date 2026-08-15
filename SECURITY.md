# Security policy

LibreWave does not have a stable release yet. Security fixes will target the current development branch until a release policy is published.

## Reporting a vulnerability

Do not open a public issue for a vulnerability that could expose local data, bypass device permissions, corrupt persistent state, or send unsafe hardware commands.

Use GitHub private vulnerability reporting after it is enabled for the repository. Until then, contact the maintainer through a private channel and include only the information needed to reproduce the issue.

Remove USB serial numbers, user names, application titles, and audio metadata from logs before sharing them.

## Hardware safety reports

Stop testing if a command causes repeated USB disconnects, unexpected device resets, bootloader enumeration, firmware prompts, or loss of normal audio enumeration. Do not retry the command. Record the exact LibreWave revision and the operation that ran immediately before the fault.

LibreWave does not accept features that update firmware, enter DFU, write flash memory, or reset a device into a recovery mode.
