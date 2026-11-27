---
name: deserialization-file-handling
description: "Deserialization and file-handling vulnerabilities per runtime: Java native serialization, Python pickle, PHP unserialize, .NET, YAML; plus archive extraction (zip-slip), XXE, path traversal, upload validation, decompression bombs. Load when the target deserializes data or processes uploaded/archives files."
---

# Deserialization and file handling

## Deserialization
- Java: `ObjectInputStream` on attacker data (gadget chains), `XMLDecoder`, Jackson `enableDefaultTyping`/polymorphic `Object` fields
- Python: `pickle.loads` on network input; `yaml.load` without `SafeLoader`; `marshal`
- PHP: `unserialize()` on user input (POP chains); .NET: `BinaryFormatter`, `TypeNameHandling` != None
- In all cases: is the type allowlisted before instantiation? Is the data authenticated (MAC) before deserialization?

## Archive extraction
- Zip-slip / tar-slip: entry names with `../` or absolute paths written outside the target dir (check canonicalization, symlink entries in archives)
- Decompression bombs: no ratio/size cap on zip/gzip/brotli
- XML: XXE — `DocumentBuilderFactory` without `disallow-doctype-decl` / external entities disabled

## File upload / path handling
- Uploaded filename used in filesystem path without sanitization (traversal, NUL bytes, unicode normalization)
- Extension/MIME allowlists that are denylist-based or checked client-side only
- Content-type sniffing: stored files served inline (`Content-Disposition: inline`, no `X-Content-Type-Options: nosniff`)
- Temporary files with predictable names in shared dirs (symlink attacks)

## When reviewing
- Trace the bytes: where does the file/data arrive, where is it parsed, where is it written/executed.
