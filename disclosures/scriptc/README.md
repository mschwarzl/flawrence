# scriptc

Flawrence found these bugs in [vercel-labs/scriptc](https://github.com/vercel-labs/scriptc), a TypeScript to native code compiler.
We reported them through the Vercel open source program on HackerOne.
The program closed all three reports.

The campaign targeted version 0.0.17.
On 2026-09-18 we checked all three again against version 0.1.2 and they were still present.

## Use after free in the HPACK decoder

HackerOne report 3898217.

The HTTP/2 server of a compiled program frees an entry of the HPACK dynamic table while it is still in use.
A remote client can corrupt request headers that are reflected in the response.
In a follow-up we showed that the bug leaks the libc base address of a production build to a remote client.
It worked in 6 of 10 connections on version 0.1.2.

## Code execution through `comptime()` during compilation

HackerOne report 4039608.

Code inside `comptime()` runs in a VM context that does not isolate it from the compiler.
It can escape that context and run commands such as `execSync` in the compiler process.
Compiling an untrusted package or project is enough.
Every release from 0.0.2 to 0.1.2 is affected.

## Native code injection through a library profile name

HackerOne report 4039644.

The compiler writes a library profile name into a comment in the generated C and LLVM code without escaping it.
A crafted name closes the comment and injects native code.
That code runs when the program is linked and executed.
Versions 0.0.5 to 0.1.2 are affected.
