# LocationTech JTS

Flawrence found this bug in [JTS](https://github.com/locationtech/jts) while it reviewed Trino.
We reported it to the Eclipse Foundation security team on 2026-05-14.
The team never responded.
The 90 day disclosure deadline has passed, so we publish it here.

The bug is fixed on master in commit [`7f7cb3f`](https://github.com/locationtech/jts/commit/7f7cb3f) from 2026-06-17.
No release contains the fix yet. The latest release is 1.20.0, and Trino still uses it.

## XXE in KMLReader

`org.locationtech.jts.io.kml.KMLReader` in jts-core creates its `XMLInputFactory` with `XMLInputFactory.newInstance()` and never disables DTDs or external entities.
None of its constructors accept a factory, so callers cannot harden it themselves.
Version 1.20.0 and the master branch at the time of the report are affected.

Any application that passes untrusted KML to `KMLReader.read()` is exposed to SSRF, file reads through parameter entities and entity expansion attacks.
Trino is one such application, see [GHSA-86m5-h93v-x9wc](../trino/README.md#geospatial-plugin).

The fix is to set these two properties on the factory.

```java
factory.setProperty(XMLInputFactory.SUPPORT_DTD, false);
factory.setProperty(XMLInputFactory.IS_SUPPORTING_EXTERNAL_ENTITIES, false);
```

KML geometry has no use for DTDs or external entities, so valid input keeps working.
A constructor that takes an `XMLInputFactory` would also help callers.

The weakness is CWE-611.
