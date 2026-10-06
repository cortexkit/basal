// The lockdown prelude and host-call bridge.
//
// The worker evaluates this file as one function expression and calls it
// once per activation, before the script is even compiled. The function
// receives the native bridge entry points as arguments (never as globals),
// rebuilds the global environment into its locked shape, freezes every
// intrinsic it can reach, and returns the few functions the worker needs to
// drive the activation. Everything the bridge relies on (the pending-call
// table, the native entry points, the captured intrinsics) lives in this
// function's closure, which the script cannot reach.
//
// Values from the host arrive as JSON text and are turned into data with the
// JSON.parse captured here. They are never evaluated as source: evaluating a
// payload like {"__proto__": {...}} as an object literal would set the
// prototype of the resulting object.
(function (native, codemode, triggerText, selfText) {
  'use strict';

  const issueOp = native.issueOp;
  const issuePrimitive = native.issuePrimitive;
  const issueSync = native.issueSync;

  // Numeric codes naming each host primitive when it is passed to the native
  // bridge. They must equal basal-proto's Primitive::code on the Rust side.
  const NOW = 1, RANDOM = 2, FACTS = 3, CLASSIFY = 4, LLM = 5, SINK_DIGEST = 6,
    SINK_STATUS = 7, KV_GET = 8, KV_SET = 9, KV_DELETE = 10, SH = 11, FS_READ = 12,
    FS_LIST = 13, FS_STAT = 14, FS_WRITE = 15, GIT_LOG = 16, GIT_REV_PARSE = 17,
    GIT_DESCRIBE_TAGS = 18, GIT_SHOW = 19, GIT_DIFF = 20, NET_FETCH = 21;

  const G = globalThis;
  const ObjectDefineProperty = Object.defineProperty;
  const ObjectGetOwnPropertyDescriptor = Object.getOwnPropertyDescriptor;
  const ObjectGetPrototypeOf = Object.getPrototypeOf;
  const ReflectApply = Reflect.apply;
  const ReflectConstruct = Reflect.construct;
  const ReflectOwnKeys = Reflect.ownKeys;
  const JSONParse = JSON.parse;
  const JSONStringify = JSON.stringify;
  const PromiseCtor = Promise;
  const promiseThen = Promise.prototype.then;
  const MapCtor = Map;
  const mapGet = Map.prototype.get;
  const mapSet = Map.prototype.set;
  const mapDelete = Map.prototype.delete;
  const WeakMapCtor = WeakMap;
  const weakMapGet = WeakMap.prototype.get;
  const weakMapSet = WeakMap.prototype.set;
  const SetCtor = Set;
  const arrayPush = Array.prototype.push;
  const NativeDate = Date;
  const dateUTC = Date.UTC;
  const dateParse = Date.parse;
  const dateProto = Date.prototype;
  const dateGetTime = Date.prototype.getTime;
  const dateToISOString = Date.prototype.toISOString;
  const regexpTest = RegExp.prototype.test;
  const StringCtor = String;
  const NumberCtor = Number;
  const ErrorCtor = Error;
  const TypeErrorCtor = TypeError;
  const RangeErrorCtor = RangeError;

  // ---- The bridge -------------------------------------------------------

  // Position -> {resolve, reject} for every asynchronous call not yet
  // settled. Positions are allocated by the worker's Rust side, which also
  // decides when an outcome is delivered; this table only routes it.
  const pending = new MapCtor();
  // Identity is private to the prelude. Script-visible names and data may
  // be changed or copied and therefore cannot identify an uncaught host error.
  const hostRejections = new WeakMapCtor();

  function encodeArgs(args) {
    const text = JSONStringify(args === undefined ? null : args);
    return text === undefined ? 'null' : text;
  }

  function hostError(data, position) {
    const message = data !== null && typeof data === 'object' && typeof data.message === 'string'
      ? data.message
      : 'host call rejected';
    const error = new ErrorCtor(message);
    ObjectDefineProperty(error, 'name', { value: 'HostError', writable: true, enumerable: false, configurable: true });
    ObjectDefineProperty(error, 'data', { value: data, writable: true, enumerable: true, configurable: true });
    ReflectApply(weakMapSet, hostRejections, [error, position]);
    return error;
  }

  function register(position) {
    let entry;
    const promise = new PromiseCtor((resolve, reject) => {
      entry = { resolve, reject };
    });
    ReflectApply(mapSet, pending, [position, entry]);
    return promise;
  }

  function callPrimitive(code, args) {
    return register(issuePrimitive(code, encodeArgs(args)));
  }

  function callSync(code) {
    const reply = issueSync(code, 'null');
    const value = JSONParse(reply[1]);
    if (!reply[0]) {
      throw hostError(value, reply[2]);
    }
    return value;
  }

  // Called by the worker to hand an outcome to the VM. Returns 0 when the
  // outcome was delivered, 1 when its text is not valid JSON, and 2 when no
  // call is pending at that position.
  function deliver(position, fulfilled, text) {
    const entry = ReflectApply(mapGet, pending, [position]);
    if (entry === undefined) {
      return 2;
    }
    ReflectApply(mapDelete, pending, [position]);
    let value;
    try {
      value = JSONParse(text);
    } catch (_) {
      return 1;
    }
    if (fulfilled) {
      entry.resolve(value);
    } else {
      entry.reject(hostError(value, position));
    }
    return 0;
  }

  // ---- Clock and randomness ----------------------------------------------

  function readClock() {
    return callSync(NOW);
  }

  // Strings are accepted only in ISO 8601 forms whose meaning does not
  // depend on the host's time zone: a date alone (which ECMAScript reads as
  // UTC) or a date-time with Z or an explicit offset.
  const ISO_DATE = /^(?:\d{4}|[+-]\d{6})-\d{2}-\d{2}(?:T\d{2}:\d{2}(?::\d{2}(?:\.\d{1,9})?)?(?:Z|[+-]\d{2}:\d{2}))?$/;

  function parseIso(text) {
    if (!ReflectApply(regexpTest, ISO_DATE, [text])) {
      return NaN;
    }
    return dateParse(text);
  }

  function timeFrom(args) {
    if (args.length === 0) {
      return readClock();
    }
    if (args.length > 1) {
      // Components are read as UTC, never as host local time.
      return ReflectApply(dateUTC, undefined, args);
    }
    const value = args[0];
    if (typeof value === 'number') {
      return value;
    }
    if (typeof value === 'string') {
      const time = parseIso(value);
      if (time !== time) {
        throw new RangeErrorCtor('Date strings must be ISO 8601 with Z or an explicit offset, or a date alone');
      }
      return time;
    }
    if (value !== null && typeof value === 'object') {
      try {
        return ReflectApply(dateGetTime, value, []);
      } catch (_) {
        // Not a Date; refused below rather than converted through strings,
        // which the native constructor would parse in local time.
      }
    }
    if (value === null || value === undefined || typeof value === 'boolean') {
      return NumberCtor(value);
    }
    throw new TypeErrorCtor('Date takes a number, an ISO 8601 string with an explicit offset, or a Date');
  }

  // The script's Date. Instances are real Dates built by the native
  // constructor, which only this closure can reach. (It is not declared as
  // `function Date`, which would shadow the native one in this closure.)
  function RunDate(...args) {
    if (new.target === undefined) {
      return ReflectApply(dateToISOString, new NativeDate(readClock()), []);
    }
    return ReflectConstruct(NativeDate, [timeFrom(args)], new.target);
  }
  ObjectDefineProperty(RunDate, 'name', { value: 'Date' });
  ObjectDefineProperty(RunDate, 'length', { value: 7 });
  ObjectDefineProperty(RunDate, 'prototype', { value: dateProto, writable: false, enumerable: false, configurable: false });
  ObjectDefineProperty(RunDate, 'now', { value: function now() { return readClock(); }, writable: true, configurable: true });
  ObjectDefineProperty(RunDate, 'UTC', { value: dateUTC, writable: true, configurable: true });
  ObjectDefineProperty(RunDate, 'parse', { value: function parse(text) { return parseIso(StringCtor(text)); }, writable: true, configurable: true });
  // Without this, `new Date(0).constructor` would hand back the native
  // constructor and with it the host's real clock.
  ObjectDefineProperty(dateProto, 'constructor', { value: RunDate, writable: true, enumerable: false, configurable: true });
  ObjectDefineProperty(G, 'Date', { value: RunDate, writable: true, enumerable: false, configurable: true });

  // Anything that reads or writes local time or formats for a locale would
  // make a flow's behaviour depend on the machine it runs on.
  const LOCAL_TIME = [
    'getDate', 'getDay', 'getFullYear', 'getHours', 'getMilliseconds', 'getMinutes', 'getMonth',
    'getSeconds', 'getYear', 'getTimezoneOffset', 'setDate', 'setFullYear', 'setHours',
    'setMilliseconds', 'setMinutes', 'setMonth', 'setSeconds', 'setYear', 'toDateString',
    'toTimeString', 'toString', 'toLocaleString', 'toLocaleDateString', 'toLocaleTimeString',
  ];
  function remove(object, key) {
    if (ObjectGetOwnPropertyDescriptor(object, key) !== undefined) {
      ObjectDefineProperty(object, key, { value: undefined, writable: false, enumerable: false, configurable: false });
    }
  }
  for (const key of LOCAL_TIME) {
    remove(dateProto, key);
  }

  const TypedArrayProto = ObjectGetPrototypeOf(Int8Array.prototype);
  for (const proto of [Object.prototype, Number.prototype, BigInt.prototype, String.prototype, Array.prototype, TypedArrayProto]) {
    for (const key of ReflectOwnKeys(proto)) {
      if (typeof key === 'string' && (key.startsWith('toLocale') || key === 'localeCompare')) {
        remove(proto, key);
      }
    }
  }

  ObjectDefineProperty(Math, 'random', {
    value: function random() { return callSync(RANDOM); },
    writable: true,
    enumerable: false,
    configurable: true,
  });

  // ---- No code from strings, no other nondeterminism ---------------------

  // Global eval and Function go below, but every function kind's prototype
  // also links to its constructor, which compiles source just the same.
  for (const fn of [function () {}, async function () {}, function* () {}, async function* () {}]) {
    ObjectDefineProperty(ObjectGetPrototypeOf(fn), 'constructor', {
      value: undefined, writable: false, enumerable: false, configurable: false,
    });
  }

  const REMOVED_GLOBALS = [
    'eval', 'Function', 'WeakRef', 'FinalizationRegistry', 'Atomics', 'SharedArrayBuffer',
    'Intl', 'performance', 'setTimeout', 'setInterval', 'clearTimeout', 'clearInterval',
    'setImmediate', 'clearImmediate', 'queueMicrotask', 'gc', 'os', 'std', 'navigator',
    'WebAssembly', 'print', 'console', 'scriptArgs', 'bjson', 'structuredClone',
  ];
  for (const key of REMOVED_GLOBALS) {
    if (!delete G[key]) {
      throw new TypeErrorCtor('lockdown could not remove global ' + key);
    }
  }

  // ---- The script's API --------------------------------------------------

  function requireName(value, what) {
    if (typeof value !== 'string' || value.length === 0) {
      throw new TypeErrorCtor(what + ' must be a non-empty string');
    }
  }

  const api = {
    ops: {
      call(module, op, args) {
        requireName(module, 'module');
        requireName(op, 'op');
        return register(issueOp(module, op, encodeArgs(args)));
      },
    },
    facts(agent, options) {
      return callPrimitive(FACTS, { agent, options });
    },
    classify(text, labels) {
      return callPrimitive(CLASSIFY, { text, labels });
    },
    llm(request) {
      return callPrimitive(LLM, request);
    },
    sink: {
      digest(agent, item, action) {
        return callPrimitive(SINK_DIGEST, { agent, item, action });
      },
      status(agent, value) {
        return callPrimitive(SINK_STATUS, { agent, value });
      },
    },
    kv: {
      get(key) {
        return callPrimitive(KV_GET, { key });
      },
      set(key, value) {
        return callPrimitive(KV_SET, { key, value });
      },
      delete(key) {
        return callPrimitive(KV_DELETE, { key });
      },
    },
    // File, git and network built-ins. The parent carries them out within
    // the manifest's fs, git and net lines; this VM never touches a file or
    // a socket itself. Each resolves to the recorded result on replay.
    fs: {
      read(path, options) {
        return callPrimitive(FS_READ, { path, options });
      },
      list(path) {
        return callPrimitive(FS_LIST, { path });
      },
      stat(path) {
        return callPrimitive(FS_STAT, { path });
      },
      write(path, text) {
        return callPrimitive(FS_WRITE, { path, text });
      },
    },
    git: {
      log(repo, options) {
        return callPrimitive(GIT_LOG, { repo, options });
      },
      revParse(repo, ref) {
        return callPrimitive(GIT_REV_PARSE, { repo, ref });
      },
      describeTags(repo, options) {
        return callPrimitive(GIT_DESCRIBE_TAGS, { repo, options });
      },
      show(repo, rev, path) {
        return callPrimitive(GIT_SHOW, { repo, rev, path });
      },
      diff(repo, from, to, options) {
        return callPrimitive(GIT_DIFF, { repo, from, to, options });
      },
    },
    net: {
      fetch(url, options) {
        return callPrimitive(NET_FETCH, { url, options });
      },
    },
    now() {
      return new PromiseCtor((resolve) => resolve(readClock()));
    },
    random() {
      return new PromiseCtor((resolve) => resolve(callSync(RANDOM)));
    },
    // A label for a group of calls. Steps are not part of the journal key:
    // concurrent callbacks have no reliable "current step".
    step(name, fn) {
      if (typeof fn !== 'function') {
        throw new TypeErrorCtor('step(name, fn) needs a function');
      }
      return fn();
    },
  };
  if (codemode) {
    api.sh = function sh(command, options) {
      return callPrimitive(SH, { command, options });
    };
  }
  for (const key of ReflectOwnKeys(api)) {
    ObjectDefineProperty(G, key, { value: api[key], writable: false, enumerable: false, configurable: false });
  }
  ObjectDefineProperty(G, 'self', {
    value: JSONParse(selfText), writable: false, enumerable: false, configurable: false,
  });
  ObjectDefineProperty(G, 'trigger', {
    value: JSONParse(triggerText), writable: false, enumerable: false, configurable: false,
  });

  // ---- Freeze ------------------------------------------------------------

  // Freezing turns every inherited data property into one that cannot be
  // shadowed by plain assignment, which breaks ordinary code such as
  // `this.name = 'MyError'` in an Error subclass. For the few properties code
  // commonly overrides, swap the data property for an accessor whose setter
  // defines an own property on the instance instead.
  function enableOverride(proto, key) {
    const desc = ObjectGetOwnPropertyDescriptor(proto, key);
    if (desc === undefined || !('value' in desc) || !desc.writable) {
      return;
    }
    const value = desc.value;
    ObjectDefineProperty(proto, key, {
      get: function () { return value; },
      set: function (next) {
        if (this === proto) {
          throw new TypeErrorCtor('cannot assign to ' + StringCtor(key) + ' of a frozen intrinsic');
        }
        ObjectDefineProperty(this, key, { value: next, writable: true, enumerable: true, configurable: true });
      },
      enumerable: desc.enumerable,
      configurable: false,
    });
  }
  for (const key of ['constructor', 'toString', 'valueOf', 'hasOwnProperty', 'isPrototypeOf', 'propertyIsEnumerable']) {
    enableOverride(Object.prototype, key);
  }
  for (const ctor of [Error, EvalError, RangeError, ReferenceError, SyntaxError, TypeError, URIError, AggregateError]) {
    for (const key of ['name', 'message', 'constructor', 'toString']) {
      enableOverride(ctor.prototype, key);
    }
  }
  enableOverride(ObjectGetPrototypeOf(function () {}), 'toString');

  // Intrinsic objects that are not the value of any global property, but
  // that a script can still obtain through syntax: the prototypes of async functions,
  // generators and the built-in iterators. The worker freezes everything
  // reachable from these and from the global object once this function
  // returns (see harden.rs), before the script is compiled.
  function* generator() {}
  async function* asyncGenerator() {}
  const roots = [
    G,
    ObjectGetPrototypeOf(async function () {}),
    ObjectGetPrototypeOf(generator),
    ObjectGetPrototypeOf(asyncGenerator),
    ObjectGetPrototypeOf(generator()),
    ObjectGetPrototypeOf(asyncGenerator()),
    ObjectGetPrototypeOf([][Symbol.iterator]()),
    ObjectGetPrototypeOf(new MapCtor()[Symbol.iterator]()),
    ObjectGetPrototypeOf(new SetCtor()[Symbol.iterator]()),
    ObjectGetPrototypeOf(''[Symbol.iterator]()),
    ObjectGetPrototypeOf(/x/g[Symbol.matchAll]('')),
    ObjectGetPrototypeOf((function () { return arguments; })()),
  ];
  if (typeof Iterator === 'function' && typeof Iterator.from === 'function') {
    ReflectApply(arrayPush, roots, [ObjectGetPrototypeOf(Iterator.from({ next() { return { done: true }; } }))]);
  }
  if (typeof [].values().map === 'function') {
    ReflectApply(arrayPush, roots, [ObjectGetPrototypeOf([].values().map((x) => x))]);
  }

  // ---- Driving the script ------------------------------------------------

  let state = 0; // 0 pending, 1 fulfilled, 2 rejected
  let resultText = '';
  let failureKind = '';
  let failureText = '';
  let failureHostPosition = null;

  // Classifies a rejection. Memory and stack exhaustion surface as ordinary
  // exceptions in QuickJS, so they are recognised by their engine messages.
  function describe(error) {
    try {
      if (error !== null && (typeof error === 'object' || typeof error === 'function')) {
        const name = error.name;
        const message = error.message;
        if (name === 'InternalError' && message === 'out of memory') {
          return ['memory', 'out of memory'];
        }
        if (name === 'RangeError' && message === 'Maximum call stack size exceeded') {
          return ['stack', message];
        }
        let text = StringCtor(name) + ': ' + StringCtor(message);
        const stack = error.stack;
        if (typeof stack === 'string' && stack.length > 0) {
          text += '\n' + stack;
        }
        return ['script', text];
      }
      return ['script', 'uncaught ' + StringCtor(error)];
    } catch (_) {
      return ['script', 'uncaught exception that could not be described'];
    }
  }

  function fail(error) {
    const position = ReflectApply(weakMapGet, hostRejections, [error]);
    const described = describe(error);
    failureKind = position === undefined ? described[0] : 'host_rejection';
    failureHostPosition = position === undefined ? null : position;
    failureText = described[1];
    state = 2;
  }

  function finish(value) {
    let text;
    try {
      text = JSONStringify(value);
    } catch (error) {
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
    ReflectApply(promiseThen, promise, [finish, fail]);
  }

  function status() {
    return [state, state === 1 ? resultText : failureKind, failureText, failureHostPosition];
  }

  return { deliver, start, status, roots };
})
