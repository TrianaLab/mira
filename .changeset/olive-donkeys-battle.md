---
"@mira/engine": patch
---

Retention's free-space floor needs bytes as well as a ratio. On a large volume
shared with everything else on the machine, 10% free is tens of gigabytes, so a
developer sitting well above any real threshold had every block deleted within a
sweep of writing it. Reclaim now requires both terms before it unlinks anything.
