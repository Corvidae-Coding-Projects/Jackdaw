//! Boa realm construction and the page-facing host environment.

use anyhow::Result;
use boa_engine::{Context, Source};

pub(crate) const SCRIPT_ENVIRONMENT: &str = r#"
(() => {
    const generation = __RWB_DOCUMENT_GENERATION__;
    const mutations = [];
    const timers = [];
    const wrappers = new Map();
    const listeners = new Map();
    const controlStates = new Map();
    const hostCompletions = new Map();
    const fetchRequests = [];
    const consoleRecords = [];
    const documentElementIds = new Set();
    const localStorageEntries = new Map();
    const sessionStorageEntries = new Map();
    const documentCookieAssignments = [];
    let documentCookie = "";
    let nextCreatedElementId = 1;
    let nextTimerId = 1;
    let nextFetchRequestId = 1;
    let nextConsoleSequence = 1;

    function storageFor(entries) {
        return Object.freeze({
            get length() { return entries.size; },
            key(index) {
                const normalized = Number(index);
                if (!Number.isInteger(normalized) || normalized < 0) {
                    return null;
                }
                return [...entries.keys()][normalized] ?? null;
            },
            getItem(key) {
                const normalized = String(key);
                return entries.has(normalized) ? entries.get(normalized) : null;
            },
            setItem(key, value) { entries.set(String(key), String(value)); },
            removeItem(key) { entries.delete(String(key)); },
            clear() { entries.clear(); },
        });
    }

    function replaceStorage(entries, replacement, storageName) {
        if (!Array.isArray(replacement)) {
            throw new TypeError(`${storageName} entries must be an array`);
        }
        entries.clear();
        for (const entry of replacement) {
            if (!Array.isArray(entry) || entry.length !== 2) {
                throw new TypeError(`${storageName} entry must contain a key and value`);
            }
            entries.set(String(entry[0]), String(entry[1]));
        }
    }

    Object.defineProperty(globalThis, "localStorage", {
        configurable: false,
        enumerable: true,
        writable: false,
        value: storageFor(localStorageEntries),
    });
    Object.defineProperty(globalThis, "sessionStorage", {
        configurable: false,
        enumerable: true,
        writable: false,
        value: storageFor(sessionStorageEntries),
    });
    Object.defineProperty(globalThis, "__rwbReplaceLocalStorage", {
        value(entries) { replaceStorage(localStorageEntries, entries, "localStorage"); },
    });
    Object.defineProperty(globalThis, "__rwbReadLocalStorage", {
        value() { return [...localStorageEntries.entries()]; },
    });
    Object.defineProperty(globalThis, "__rwbReplaceSessionStorage", {
        value(entries) { replaceStorage(sessionStorageEntries, entries, "sessionStorage"); },
    });
    Object.defineProperty(globalThis, "__rwbReadSessionStorage", {
        value() { return [...sessionStorageEntries.entries()]; },
    });

    function queue(kind, elementId, value = "", propertyName = "") {
        mutations.push({
            kind,
            generation,
            element_id: elementId,
            value: String(value),
            property_name: String(propertyName),
        });
    }

    function wrapperFor(elementId) {
        let engineId = String(elementId);
        let wrapper = wrappers.get(engineId);
        if (wrapper !== undefined) {
            return wrapper;
        }

        wrapper = {};
        Object.defineProperty(wrapper, "__rwbEngineId", {
            get() { return engineId; },
        });
        Object.defineProperty(wrapper, "id", {
            enumerable: true,
            get() { return engineId.startsWith("__rwb_created_") ? "" : engineId; },
            set(value) {
                const nextId = String(value);
                if (nextId.length === 0) {
                    throw new TypeError("element id must not be empty");
                }
                const previousId = engineId;
                queue("set_id", previousId, nextId);
                wrappers.delete(previousId);
                documentElementIds.delete(previousId);
                engineId = nextId;
                wrappers.set(engineId, wrapper);
                documentElementIds.add(engineId);
            },
        });
        Object.defineProperty(wrapper, "textContent", {
            enumerable: true,
            set(value) { queue("set_text", engineId, value); },
        });
        Object.defineProperty(wrapper, "value", {
            enumerable: true,
            get() { return controlStates.get(engineId)?.value ?? ""; },
        });
        Object.defineProperty(wrapper, "checked", {
            enumerable: true,
            get() { return controlStates.get(engineId)?.checked ?? false; },
        });
        Object.defineProperty(wrapper, "style", {
            enumerable: true,
            value: Object.freeze({
                setProperty(name, value) { queue("set_style", engineId, value, name); },
            }),
        });
        Object.defineProperty(wrapper, "appendChild", {
            value(child) {
                if (child === null || typeof child !== "object" ||
                    typeof child.__rwbEngineId !== "string") {
                    throw new TypeError("appendChild requires a live element wrapper");
                }
                queue("append_child", engineId, child.__rwbEngineId);
                return child;
            },
        });
        Object.defineProperty(wrapper, "remove", {
            value() {
                queue("remove", engineId);
                documentElementIds.delete(engineId);
            },
        });
        Object.defineProperty(wrapper, "addEventListener", {
            value(type, callback, options = {}) {
                if (typeof callback !== "function") {
                    throw new TypeError("event listener must be a function");
                }
                const key = `${engineId}:${String(type)}`;
                const entries = listeners.get(key) ?? [];
                entries.push({
                    callback,
                    capture: options === true || Boolean(options.capture),
                    once: options === true ? false : Boolean(options.once),
                });
                listeners.set(key, entries);
            },
        });
        Object.defineProperty(wrapper, "removeEventListener", {
            value(type, callback) {
                const key = `${engineId}:${String(type)}`;
                const entries = listeners.get(key);
                if (entries === undefined) {
                    return;
                }
                listeners.set(key, entries.filter((entry) => entry.callback !== callback));
            },
        });
        wrappers.set(engineId, wrapper);
        return wrapper;
    }

    Object.defineProperty(globalThis, "__rwbRegisterElementIds", {
        value(elementIds) {
            documentElementIds.clear();
            for (const elementId of elementIds) {
                documentElementIds.add(String(elementId));
            }
        },
    });

    const documentObject = {
        getElementById(elementId) {
            const normalized = String(elementId);
            return documentElementIds.has(normalized) ? wrapperFor(normalized) : null;
        },
        querySelector(selector) {
            const normalized = String(selector).trim();
            if (/^#[A-Za-z_][A-Za-z0-9_.:-]*$/.test(normalized)) {
                return this.getElementById(normalized.slice(1));
            }
            throw new SyntaxError("this selector is not yet supported");
        },
        querySelectorAll(selector) {
            const match = this.querySelector(selector);
            return Object.freeze(match === null ? [] : [match]);
        },
        createElement(tagName) {
            const normalized = String(tagName).toLowerCase();
            if (!/^[a-z][a-z0-9-]*$/.test(normalized)) {
                throw new TypeError("unsupported HTML tag name");
            }
            const internalId = `__rwb_created_${nextCreatedElementId++}`;
            queue("create_element", internalId, normalized);
            documentElementIds.add(internalId);
            return wrapperFor(internalId);
        },
    };
    Object.defineProperty(documentObject, "cookie", {
        enumerable: true,
        get() { return documentCookie; },
        set(value) { documentCookieAssignments.push(String(value)); },
    });
    Object.freeze(documentObject);
    Object.defineProperty(globalThis, "document", {
        configurable: false,
        enumerable: true,
        writable: false,
        value: documentObject,
    });
    Object.defineProperty(globalThis, "__rwbReplaceDocumentCookie", {
        value(value) { documentCookie = String(value); },
    });
    Object.defineProperty(globalThis, "__rwbTakeDocumentCookieAssignments", {
        value() { return documentCookieAssignments.splice(0); },
    });

    function consoleText(values) {
        return values.map((value) => {
            if (typeof value === "string") {
                return value;
            }
            try {
                const encoded = JSON.stringify(value);
                return encoded === undefined ? String(value) : encoded;
            } catch (_) {
                return String(value);
            }
        }).join(" " );
    }

    function recordConsole(level, values) {
        consoleRecords.push({
            sequence: nextConsoleSequence++,
            level,
            text: consoleText(values),
        });
        if (consoleRecords.length > 256) {
            consoleRecords.shift();
        }
    }

    Object.defineProperty(globalThis, "console", {
        configurable: false,
        enumerable: true,
        writable: false,
        value: Object.freeze({
            debug(...values) { recordConsole("debug", values); },
            log(...values) { recordConsole("log", values); },
            info(...values) { recordConsole("info", values); },
            warn(...values) { recordConsole("warn", values); },
            error(...values) { recordConsole("error", values); },
        }),
    });
    Object.defineProperty(globalThis, "__rwbReadConsoleRecords", {
        value() { return consoleRecords.slice(); },
    });

    globalThis.setTimeout = (callback, delay = 0, ...args) => {
        if (typeof callback !== "function") {
            throw new TypeError("setTimeout callback must be a function");
        }
        const numericDelay = Number(delay);
        const delayMilliseconds = Number.isFinite(numericDelay)
            ? Math.max(0, numericDelay)
            : 0;
        const id = nextTimerId++;
        timers.push({
            id,
            generation,
            callback,
            args,
            dueTimeMilliseconds: Date.now() + delayMilliseconds,
        });
        return id;
    };

    globalThis.clearTimeout = (id) => {
        const index = timers.findIndex((timer) => timer.id === id);
        if (index >= 0) {
            timers.splice(index, 1);
        }
    };

    Object.defineProperty(globalThis, "__rwbAwaitHostCompletion", {
        value(requestId) {
            const id = String(requestId);
            if (id.length === 0) {
                throw new TypeError("host completion ID must not be empty");
            }
            const existing = hostCompletions.get(id);
            if (existing !== undefined) {
                return existing.promise;
            }
            let resolveCompletion;
            let rejectCompletion;
            const promise = new Promise((resolve, reject) => {
                resolveCompletion = resolve;
                rejectCompletion = reject;
            });
            hostCompletions.set(id, { promise, resolveCompletion, rejectCompletion });
            return promise;
        },
    });
    Object.defineProperty(globalThis, "__rwbCompleteHostRequest", {
        value(requestId, value) {
            const id = String(requestId);
            const completion = hostCompletions.get(id);
            if (completion === undefined) {
                return false;
            }
            hostCompletions.delete(id);
            completion.resolveCompletion(String(value));
            return true;
        },
    });
    Object.defineProperty(globalThis, "__rwbRejectHostRequest", {
        value(requestId, message) {
            const id = String(requestId);
            const completion = hostCompletions.get(id);
            if (completion === undefined) {
                return false;
            }
            hostCompletions.delete(id);
            completion.rejectCompletion(new TypeError(String(message)));
            return true;
        },
    });

    globalThis.fetch = (input) => {
        const requestId = `fetch_${nextFetchRequestId++}`;
        fetchRequests.push({
            generation,
            request_id: requestId,
            url: String(input),
        });
        return globalThis.__rwbAwaitHostCompletion(requestId).then((serialized) => {
            const payload = JSON.parse(serialized);
            const body = String(payload.body);
            const status = Number(payload.status);
            return Object.freeze({
                ok: status >= 200 && status <= 299,
                status,
                url: String(payload.url),
                text() { return Promise.resolve(body); },
            });
        });
    };
    Object.defineProperty(globalThis, "__rwbTakeFetchRequests", {
        value() { return fetchRequests.splice(0, fetchRequests.length); },
    });

    Object.defineProperty(globalThis, "__rwbSetControlState", {
        value(elementId, value, checked) {
            controlStates.set(String(elementId), {
                value: String(value),
                checked: Boolean(checked),
            });
        },
    });
    Object.defineProperty(globalThis, "__rwbTakeMutations", {
        value() { return mutations.splice(0, mutations.length); },
    });
    Object.defineProperty(globalThis, "__rwbRunNextTimer", {
        value() {
            const now = Date.now();
            const index = timers.findIndex((timer) =>
                timer.generation === generation && timer.dueTimeMilliseconds <= now
            );
            if (index < 0) {
                return false;
            }
            const [timer] = timers.splice(index, 1);
            timer.callback(...timer.args);
            return true;
        },
    });
    Object.defineProperty(globalThis, "__rwbDispatchEvent", {
        value(path, type, bubbles = true, details = {}) {
            if (!Array.isArray(path) || path.length === 0) {
                throw new TypeError("event path must contain a target");
            }
            const targetId = String(path[0]);
            const composedPathIds = path.map((elementId) => String(elementId));
            let currentTarget = null;
            let eventPhase = 0;
            let defaultPrevented = false;
            let propagationStopped = false;
            let immediateStopped = false;
            const event = {
                type: String(type),
                key: details.key === undefined ? "" : String(details.key),
                get target() { return wrapperFor(targetId); },
                get currentTarget() { return currentTarget; },
                get eventPhase() { return eventPhase; },
                get defaultPrevented() { return defaultPrevented; },
                preventDefault() { defaultPrevented = true; },
                stopPropagation() { propagationStopped = true; },
                stopImmediatePropagation() {
                    immediateStopped = true;
                    propagationStopped = true;
                },
                composedPath() {
                    return composedPathIds.map((elementId) => wrapperFor(elementId));
                },
            };
            const invoke = (elementId, phase, capture) => {
                const wrapper = wrapperFor(elementId);
                const key = `${String(elementId)}:${String(type)}`;
                const entries = [...(listeners.get(key) ?? [])];
                currentTarget = wrapper;
                eventPhase = phase;
                immediateStopped = false;
                for (const entry of entries) {
                    if (entry.capture !== capture) {
                        continue;
                    }
                    entry.callback.call(wrapper, event);
                    if (entry.once) {
                        const current = listeners.get(key) ?? [];
                        const index = current.indexOf(entry);
                        if (index >= 0) {
                            current.splice(index, 1);
                        }
                    }
                    if (immediateStopped) {
                        break;
                    }
                }
            };
            for (let index = path.length - 1; index > 0; index -= 1) {
                invoke(path[index], 1, true);
                if (propagationStopped) {
                    break;
                }
            }
            if (!propagationStopped) {
                invoke(path[0], 2, true);
                if (!immediateStopped) {
                    invoke(path[0], 2, false);
                }
            }
            if (bubbles && !propagationStopped) {
                for (let index = 1; index < path.length; index += 1) {
                    invoke(path[index], 3, false);
                    if (propagationStopped) {
                        break;
                    }
                }
            }
            currentTarget = null;
            eventPhase = 0;
            return !defaultPrevented;
        },
    });
})();
"#;

pub(crate) const MAX_SCRIPT_LOOP_ITERATIONS: u64 = 100_000;

pub(crate) fn new_context(generation: u64) -> Result<Context> {
    let mut context = Context::default();
    context
        .runtime_limits_mut()
        .set_loop_iteration_limit(MAX_SCRIPT_LOOP_ITERATIONS);
    let source = SCRIPT_ENVIRONMENT.replace("__RWB_DOCUMENT_GENERATION__", &generation.to_string());
    context
        .eval(Source::from_bytes(&source))
        .map_err(|error| anyhow::anyhow!("could not initialize document script realm: {error}"))?;
    Ok(context)
}
