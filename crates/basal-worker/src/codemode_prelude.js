// The codemode environment has only catalog tools and console output. Native
// entry points and pending promises stay private to this activation's closure.
(function (native, names) {
  'use strict';
  const issueTool = native.issueTool;
  const emitConsole = native.console;
  const G = globalThis;
  const define = Object.defineProperty;
  const descriptor = Object.getOwnPropertyDescriptor;
  const prototypeOf = Object.getPrototypeOf;
  const apply = Reflect.apply;
  const construct = Reflect.construct;
  const ownKeys = Reflect.ownKeys;
  const parse = JSON.parse;
  const stringify = JSON.stringify;
  const StringCtor = String;
  const NumberCtor = Number;
  const ErrorCtor = Error;
  const TypeErrorCtor = TypeError;
  const RangeErrorCtor = RangeError;
  const PromiseCtor = Promise;
  const then = Promise.prototype.then;
  const MapCtor = Map;
  const SetCtor = Set;
  const mapGet = Map.prototype.get;
  const mapSet = Map.prototype.set;
  const mapDelete = Map.prototype.delete;
  const WeakMapCtor = WeakMap;
  const weakGet = WeakMap.prototype.get;
  const weakSet = WeakMap.prototype.set;
  const arrayPush = Array.prototype.push;
  const NativeDate = Date;
  const nativeNow = Date.now;
  const dateUTC = Date.UTC;
  const dateParse = Date.parse;
  const dateProto = Date.prototype;
  const dateGetTime = Date.prototype.getTime;
  const dateToISOString = Date.prototype.toISOString;
  const regexpTest = RegExp.prototype.test;

  // QuickJS stores Error.prepareStackTrace and Error.stackTraceLimit in context
  // state. Rust installed a stack-budget observer before evaluating this file.
  // Hiding both setters prevents its replacement and keeps CallSites that could
  // expose private bridge functions out of the script's reach.
  for (const key of ['prepareStackTrace', 'stackTraceLimit']) {
    define(ErrorCtor, key, { value: undefined, writable: false, enumerable: false, configurable: false });
  }

  const pending = new MapCtor();
  const hostRejections = new WeakMapCtor();
  function register(position, tool) {
    let entry;
    const promise = new PromiseCtor((resolve, reject) => { entry = { resolve, reject, tool }; });
    apply(mapSet, pending, [position, entry]);
    return promise;
  }
  function hostError(data, position, tool) {
    const error = new ErrorCtor(data !== null && typeof data === 'object' && typeof data.message === 'string'
      ? data.message : 'tool call rejected');
    define(error, 'name', { value: 'HostError', writable: true, configurable: true });
    define(error, 'data', { value: data, writable: true, enumerable: true, configurable: true });
    for (const key of ['code', 'outcome']) {
      define(error, key, { value: data !== null && typeof data === 'object' ? data[key] : undefined,
        writable: true, enumerable: true, configurable: true });
    }
    define(error, 'tool', { value: tool, writable: true, enumerable: true, configurable: true });
    apply(weakSet, hostRejections, [error, position]);
    return error;
  }
  function deliver(position, fulfilled, text) {
    const entry = apply(mapGet, pending, [position]);
    if (entry === undefined) return 2;
    apply(mapDelete, pending, [position]);
    let value;
    try { value = parse(text); } catch (_) { return 1; }
    if (fulfilled) entry.resolve(value);
    else entry.reject(hostError(value, position, entry.tool));
    return 0;
  }

  // Native clock and random sources stay inside the worker. Date strings accept
  // ISO dates alone (UTC) or ISO date-times with Z or an explicit offset. Numeric
  // component construction also uses UTC, never the worker's local time zone.
  const ISO_DATE = /^(?:\d{4}|[+-]\d{6})-\d{2}-\d{2}(?:T\d{2}:\d{2}(?::\d{2}(?:\.\d{1,9})?)?(?:Z|[+-]\d{2}:\d{2}))?$/;
  function parseIso(text) {
    return apply(regexpTest, ISO_DATE, [text]) ? dateParse(text) : NaN;
  }
  function timeFrom(args) {
    if (args.length === 0) return nativeNow();
    if (args.length > 1) return apply(dateUTC, undefined, args);
    const value = args[0];
    if (typeof value === 'number') return value;
    if (typeof value === 'string') {
      const time = parseIso(value);
      if (time !== time) throw new RangeErrorCtor('Date strings must be ISO 8601 with Z or an explicit offset, or a date alone');
      return time;
    }
    if (value !== null && typeof value === 'object') {
      try { return apply(dateGetTime, value, []); } catch (_) { }
    }
    if (value === null || value === undefined || typeof value === 'boolean') return NumberCtor(value);
    throw new TypeErrorCtor('Date takes a number, an ISO 8601 string with an explicit offset, or a Date');
  }
  function RunDate(...args) {
    if (new.target === undefined) return apply(dateToISOString, new NativeDate(nativeNow()), []);
    return construct(NativeDate, [timeFrom(args)], new.target);
  }
  define(RunDate, 'name', { value: 'Date' });
  define(RunDate, 'length', { value: 7 });
  define(RunDate, 'prototype', { value: dateProto, writable: false });
  define(RunDate, 'now', { value: nativeNow, writable: true, configurable: true });
  define(RunDate, 'UTC', { value: dateUTC, writable: true, configurable: true });
  define(RunDate, 'parse', { value: function parse(text) { return parseIso(StringCtor(text)); }, writable: true, configurable: true });
  define(dateProto, 'constructor', { value: RunDate, writable: true, configurable: true });
  define(G, 'Date', { value: RunDate, writable: true, configurable: true });
  function remove(object, key) {
    if (descriptor(object, key) !== undefined) {
      define(object, key, { value: undefined, writable: false, enumerable: false, configurable: false });
    }
  }
  const LOCAL_TIME = [
    'getDate', 'getDay', 'getFullYear', 'getHours', 'getMilliseconds', 'getMinutes', 'getMonth',
    'getSeconds', 'getYear', 'getTimezoneOffset', 'setDate', 'setFullYear', 'setHours',
    'setMilliseconds', 'setMinutes', 'setMonth', 'setSeconds', 'setYear', 'toDateString',
    'toTimeString', 'toString', 'toLocaleString', 'toLocaleDateString', 'toLocaleTimeString',
  ];
  for (const key of LOCAL_TIME) remove(dateProto, key);
  const TypedArrayProto = prototypeOf(Int8Array.prototype);
  for (const proto of [Object.prototype, Number.prototype, BigInt.prototype, String.prototype, Array.prototype, TypedArrayProto]) {
    for (const key of ownKeys(proto)) {
      if (typeof key === 'string' && (key.startsWith('toLocale') || key === 'localeCompare')) remove(proto, key);
    }
  }
  for (const fn of [function () {}, async function () {}, function* () {}, async function* () {}]) {
    define(prototypeOf(fn), 'constructor', { value: undefined, writable: false, configurable: false });
  }
  const REMOVED_GLOBALS = [
    'eval', 'Function', 'WeakRef', 'FinalizationRegistry', 'Atomics', 'SharedArrayBuffer',
    'Intl', 'performance', 'setTimeout', 'setInterval', 'clearTimeout', 'clearInterval',
    'setImmediate', 'clearImmediate', 'queueMicrotask', 'gc', 'os', 'std', 'navigator',
    'WebAssembly', 'print', 'console', 'scriptArgs', 'bjson', 'structuredClone',
  ];
  for (const key of REMOVED_GLOBALS) {
    if (!delete G[key]) throw new TypeErrorCtor('lockdown could not remove global ' + key);
  }

  const tools = Object.create(null);
  for (const name of names) {
    define(tools, name, { value: async function (input) {
      const text = stringify(input === undefined ? null : input);
      return register(issueTool(name, text === undefined ? 'null' : text), name);
    }, enumerable: true });
  }
  define(G, 'tools', { value: tools, writable: false, configurable: false });
  const console = Object.create(null);
  define(console, 'log', { value: function log(...args) {
    const parts = [];
    for (const arg of args) {
      let text = arg;
      if (typeof arg !== 'string') {
        try { text = stringify(arg); } catch (_) { text = undefined; }
        if (text === undefined) text = StringCtor(arg);
      }
      apply(arrayPush, parts, [text]);
    }
    emitConsole(parts.join(' ') + '\n');
  }, enumerable: true });
  define(G, 'console', { value: console, writable: false, configurable: false });

  // Permit ordinary Error subclasses and own Object-method overrides without
  // making the shared prototypes writable after the worker hardens the roots.
  function enableOverride(proto, key) {
    const desc = descriptor(proto, key);
    if (desc === undefined || !('value' in desc) || !desc.writable) return;
    const value = desc.value;
    define(proto, key, {
      get: function () { return value; },
      set: function (next) {
        if (this === proto) throw new TypeErrorCtor('cannot assign to ' + StringCtor(key) + ' of a frozen intrinsic');
        define(this, key, { value: next, writable: true, enumerable: true, configurable: true });
      }, enumerable: desc.enumerable, configurable: false,
    });
  }
  for (const key of ['constructor', 'toString', 'valueOf', 'hasOwnProperty', 'isPrototypeOf', 'propertyIsEnumerable']) enableOverride(Object.prototype, key);
  for (const ctor of [Error, EvalError, RangeError, ReferenceError, SyntaxError, TypeError, URIError, AggregateError]) {
    for (const key of ['name', 'message', 'constructor', 'toString']) enableOverride(ctor.prototype, key);
  }
  enableOverride(prototypeOf(function () {}), 'toString');
  function* generator() {}
  async function* asyncGenerator() {}
  const roots = [G, prototypeOf(async function () {}), prototypeOf(generator), prototypeOf(asyncGenerator),
    prototypeOf(generator()), prototypeOf(asyncGenerator()), prototypeOf([][Symbol.iterator]()),
    prototypeOf(new MapCtor()[Symbol.iterator]()), prototypeOf(new SetCtor()[Symbol.iterator]()),
    prototypeOf(''[Symbol.iterator]()), prototypeOf(/x/g[Symbol.matchAll]('')),
    prototypeOf((function () { return arguments; })()),
  ];
  if (typeof Iterator === 'function' && typeof Iterator.from === 'function') {
    apply(arrayPush, roots, [prototypeOf(Iterator.from({ next() { return { done: true }; } }))]);
  }
  if (typeof [].values().map === 'function') apply(arrayPush, roots, [prototypeOf([].values().map((x) => x))]);

  let state = 0;
  let resultText = '';
  let failureKind = '';
  let failureText = '';
  let failureHostPosition = null;
  const errorPrototypes = [Error, EvalError, RangeError, ReferenceError, SyntaxError,
    TypeError, URIError, AggregateError, InternalError].map(ctor => [ctor.prototype, ctor.name]);
  function ownString(object, key) {
    const desc = descriptor(object, key);
    return desc !== undefined && typeof desc.value === 'string' ? desc.value : undefined;
  }
  function describe(error) {
    try {
      if (error !== null && (typeof error === 'object' || typeof error === 'function')) {
        const proto = prototypeOf(error);
        const known = errorPrototypes.find(entry => entry[0] === proto);
        const name = ownString(error, 'name') || (known === undefined ? 'Error' : known[1]);
        const message = ownString(error, 'message') || '';
        if (name === 'RangeError' && message === 'Maximum call stack size exceeded') return ['stack', message];
        let text = name + ': ' + message;
        const stack = ownString(error, 'stack');
        if (typeof stack === 'string' && stack.length > 0) text += '\n' + stack;
        return ['script', text];
      }
      return ['script', 'uncaught ' + (typeof error === 'string' ? error : typeof error)];
    } catch (_) { return ['script', 'uncaught exception that could not be described']; }
  }
  function fail(error) {
    const position = apply(weakGet, hostRejections, [error]);
    const described = describe(error);
    failureKind = position === undefined ? described[0] : 'host_rejection';
    failureHostPosition = position === undefined ? null : position;
    failureText = described[1];
    state = 2;
  }
  function finish(value) {
    let text;
    try { text = stringify(value); } catch (error) {
      failureKind = 'unserializable';
      failureText = describe(error)[1];
      state = 2;
      return;
    }
    resultText = text === undefined ? 'null' : text;
    state = 1;
  }
  function start(main) {
    const promise = new PromiseCtor((resolve) => resolve(main()));
    apply(then, promise, [finish, fail]);
  }
  function status() { return [state, state === 1 ? resultText : failureKind, failureText, failureHostPosition]; }
  return { deliver, start, status, roots };
})
