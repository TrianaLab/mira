---
"@mira/engine": patch
"@mira/operator": patch
---

`verify-release` proved a published chart names a published image by grepping
rendered YAML for an unquoted string, and the template renders it quoted.
