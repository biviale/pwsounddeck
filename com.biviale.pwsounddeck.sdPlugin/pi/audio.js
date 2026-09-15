let websocket = null;
let pluginAction = null;
let pluginUUID = null;
let instanceContext = null;
let globalActionInfo = null;

// The standard OpenAction/StreamDeck plugin registration hook for Property Inspectors
function connectElgatoStreamDeckSocket(inPort, inPropertyInspectorUUID, inRegisterEvent, inInfo, inActionInfo) {
    pluginUUID = inPropertyInspectorUUID;
    globalActionInfo = JSON.parse(inActionInfo);
    pluginAction = globalActionInfo.action;
    instanceContext = globalActionInfo.context;

    websocket = new WebSocket('ws://127.0.0.1:' + inPort);

    websocket.onopen = function () {
        // Register property inspector
        const json = {
            "event": inRegisterEvent,
            "uuid": inPropertyInspectorUUID
        };
        websocket.send(JSON.stringify(json));

        // Request device list from plugin
        sendToPlugin({ command: "get_devices" });

        // Populate settings if they already exist
        if (globalActionInfo.payload && globalActionInfo.payload.settings) {
            updateSettingsUI(globalActionInfo.payload.settings);
        }
    };

    websocket.onmessage = function (evt) {
        const jsonObj = JSON.parse(evt.data);
        const event = jsonObj.event;

        if (event === "sendToPropertyInspector") {
            const payload = jsonObj.payload;
            if (payload.event === "device_list") {
                updateDeviceList(payload.devices);
            } else if (payload.event === "file_selected") {
                document.getElementById('audioPath').value = payload.path;
                saveSettings();
            }
        } else if (event === "didReceiveSettings") {
            updateSettingsUI(jsonObj.payload.settings);
        }
    };
}

function sendToPlugin(payload) {
    if (websocket) {
        websocket.send(JSON.stringify({
            "event": "sendToPlugin",
            "action": pluginAction,
            "context": instanceContext,
            "payload": payload
        }));
    }
}

function saveSettings() {
    if (websocket) {
        const payload = {
            path: document.getElementById('audioPath').value,
            device: document.getElementById('audioDevice').value,
            playback_mode: document.getElementById('playbackMode').value,
            volume: document.getElementById('volume').value
        };
        websocket.send(JSON.stringify({
            "event": "setSettings",
            "context": instanceContext,
            "payload": payload
        }));
    }
}

function updateSettingsUI(settings) {
    if (settings.path) {
        document.getElementById('audioPath').value = settings.path;
    }
    if (settings.device) {
        document.getElementById('audioDevice').value = settings.device;
    }
    if (settings.playback_mode) {
        document.getElementById('playbackMode').value = settings.playback_mode;
    }
    if (settings.volume !== undefined) {
        document.getElementById('volume').value = settings.volume;
        document.getElementById('volumeValue').textContent = settings.volume + "%";
    }
}

function updateDeviceList(devices) {
    const select = document.getElementById('audioDevice');

    // Determine the target device to select.
    // If the select currently has a non-default value, preserve it.
    // Otherwise, try to use the initial setting if it exists.
    let targetVal = select.value;
    if (!targetVal && globalActionInfo && globalActionInfo.payload && globalActionInfo.payload.settings && globalActionInfo.payload.settings.device) {
        targetVal = globalActionInfo.payload.settings.device;
    }

    // Clear existing options except Default
    select.innerHTML = '<option value="">Default OS Device</option>';

    devices.forEach(device => {
        const option = document.createElement("option");
        // device is now an object { name: "internal_pa_name", description: "Human Readable Name" }
        option.value = device.name;
        option.text = device.description;
        select.appendChild(option);
    });

    select.value = targetVal;
}

// Event listeners
document.getElementById('audioPath').addEventListener('change', saveSettings);
document.getElementById('audioDevice').addEventListener('change', saveSettings);
document.getElementById('playbackMode').addEventListener('change', saveSettings);
document.getElementById('volume').addEventListener('input', function () {
    document.getElementById('volumeValue').textContent = this.value + "%";
});
document.getElementById('volume').addEventListener('change', saveSettings);

document.getElementById('browseBtn').addEventListener('click', function () {
    sendToPlugin({ command: "open_file_picker" });
});

document.getElementById('refreshBtn').addEventListener('click', function () {
    sendToPlugin({ command: "get_devices" });
});
