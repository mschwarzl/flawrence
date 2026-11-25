# txiki.js

Flawrence found this bug in [txiki.js](https://github.com/saghul/txiki.js).
We reported it privately as GHSA-gfvw-rpjq-557g.
It is not fixed, so we publish the report here.
The root cause is in quickjs-ng, see [disclosures/quickjs-ng](../quickjs-ng/README.md).

## Out-of-bounds write through FileHandle.read()

txiki.js v26.6.0 is affected by a JavaScript-reachable native out-of-bounds write through the public `FileHandle.read()` API.

The bundled quickjs-ng commit bf8988fc401e737f9946cd10a3463b48aab0fd7e returns a stale creation-time size from `JS_GetUint8Array()` after a length-tracking Uint8Array's resizable backing buffer shrinks. JavaScript correctly sees a one-byte view, but src/utils.c:242 receives the pointer to that one-byte allocation with the old 65,536-byte size. src/mod_fs.c:445 forwards both values to `uv_buf_t`, and src/mod_fs.c:451 lets `uv_fs_read()` write the stale size.

Ordinary-JavaScript reproducer:

```js
const rab = new ArrayBuffer(65536, { maxByteLength: 65536 });
const view = new Uint8Array(rab);
rab.resize(1);
console.log(rab.byteLength, view.byteLength); // 1 1

const file = await tjs.open('/dev/zero', 'r');
try {
    await file.read(view);
} finally {
    await file.close();
}
```

Build v26.6.0 with `BUILD_WITH_ASAN=ON` and `BUILD_WITH_MIMALLOC=OFF`, then run the script. Across 10/10 runs, the process aborted with a native memory-safety failure: nine direct ASan heap-buffer-overflow writes and one downstream invalid free after the overwrite. A representative report is:

```text
ERROR: AddressSanitizer: heap-buffer-overflow
WRITE of size 41136
  #0 read
  #1 uv__fs_read deps/libuv/src/unix/fs.c:538
  #2 uv__fs_work deps/libuv/src/unix/fs.c:1738

address is 0 bytes after 1-byte region
allocated by js_array_buffer_resize deps/quickjs/quickjs.c:58342
```

The input bytes are controlled by the readable file. No private module, FFI, malformed bytecode, or C harness is needed.

There is also a public OOB-read path through `WebAssembly.validate(view)`: src/wasm.c obtains the same stale size and `memcpy()` reads 65,536 bytes from the resized allocation. It reproduced in 10/10 runs on v26.6.0 and current txiki.js master 59eab48.

The root defect remains present in current quickjs-ng master 82626eacc81825340736af5a6f363f5ebdf8aede and has been privately reported for coordinated remediation as GHSA-3pwq-gpm6-j3mf.

### Fix

The tested quickjs-ng fix is to return `p->u.array.count` from `JS_GetUint8Array()` and derive `JS_GetTypedArrayBuffer()` byte length from the live count. With that fix applied to the v26.6.0 submodule, both PoCs exited cleanly in 10/10 runs. As a txiki.js-side defense, `tjs_buf_ref_get()` can additionally clamp the returned view size to the current backing ArrayBuffer bounds before handing the buffer to asynchronous native I/O.

Classification: CWE-787. Confirmed affected release: v26.6.0.
