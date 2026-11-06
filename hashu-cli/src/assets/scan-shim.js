// Node globals that bc-ur's dependencies expect, set before they load.
import { Buffer } from 'buffer'
globalThis.Buffer = globalThis.Buffer || Buffer
globalThis.process = globalThis.process || { env: {}, browser: true, version: '', versions: {}, nextTick: (f, ...a) => Promise.resolve().then(() => f(...a)) }
