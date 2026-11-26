--[[

DirectionPolicy

Validates directional negotiation independently of optional debug type checks. Direction
controls traffic, never conflict authority or persistent user settings.

--]]

const DIRECTIONS = { "fs-to-studio", "studio-to-fs", "bidirectional" }
const DirectionPolicy = {}

function DirectionPolicy.isDirection(value)
	return type(value) == "string" and table.find(DIRECTIONS, value) ~= nil
end

function DirectionPolicy.validateServer(info)
	assert(type(info) == "table", "Server info must be a table.")
	if info.supportedSyncDirections ~= nil then
		assert(type(info.supportedSyncDirections) == "table", "Invalid supported sync directions.")
		-- Unknown entries are ignored so a server that adds directions stays compatible.
		const seen = {}
		for key, direction in info.supportedSyncDirections do
			assert(
				type(key) == "number" and key % 1 == 0 and key >= 1 and type(direction) == "string",
				"Invalid supported sync directions."
			)
			seen[direction] = true
		end
		for _, direction in DIRECTIONS do
			assert(seen[direction], "The server does not support the directional sync contract.")
		end
	end
	if info.syncDirection ~= nil then
		assert(
			type(info.sessionId) == "string" and #info.sessionId > 0,
			"Directional sync requires a server session identity."
		)
		assert(DirectionPolicy.isDirection(info.syncDirection), "Invalid server sync direction.")
		assert(info.supportedSyncDirections ~= nil, "The server did not advertise directional sync support.")
	end
end

function DirectionPolicy.marker(url, sessionId, direction)
	if url == nil and sessionId == nil and direction == nil then
		return nil
	end
	assert(type(url) == "string", "The live edit URL marker is missing or malformed.")
	const host, port = url:match("^(.-):(%d+)$")
	assert(
		host ~= nil and #host > 0 and tonumber(port) > 0 and tonumber(port) <= 65535,
		"The live edit URL marker is malformed."
	)
	assert((sessionId == nil) == (direction == nil), "The live edit session and direction markers are incomplete.")
	if sessionId ~= nil then
		assert(type(sessionId) == "string" and #sessionId > 0, "The live edit session marker is malformed.")
		assert(DirectionPolicy.isDirection(direction), "The live edit direction marker is malformed.")
	end
	return { host = host, port = port, sessionId = sessionId, direction = direction }
end

function DirectionPolicy.negotiate(info, marker, twoWaySync)
	DirectionPolicy.validateServer(info)
	assert(type(twoWaySync) == "boolean", "Two-way sync must be a boolean.")
	if marker ~= nil then
		assert(
			type(marker) == "table" and type(marker.host) == "string" and type(marker.port) == "string",
			"The expected live edit marker is malformed."
		)
		DirectionPolicy.marker(marker.host .. ":" .. marker.port, marker.sessionId, marker.direction)
		if marker.sessionId ~= nil then
			assert(
				marker.sessionId == info.sessionId,
				"The live edit server changed identity. Rebuild a fresh place; the current place and files were preserved."
			)
			assert(
				marker.direction == info.syncDirection,
				"The live edit direction does not match the server policy. Rebuild a fresh place."
			)
		elseif info.syncDirection ~= nil then
			error("This server requires complete live edit session and direction markers. Rebuild a fresh place.", 0)
		end
	end
	const direction = info.syncDirection or (if marker ~= nil or twoWaySync then "bidirectional" else "fs-to-studio")
	if direction == "studio-to-fs" then
		assert(
			marker ~= nil and marker.sessionId ~= nil,
			"Studio-only sync requires a freshly generated live edit place with session and direction markers."
		)
	end
	return direction
end

function DirectionPolicy.allowsPublication(direction, origin)
	if direction ~= "studio-to-fs" then
		return true
	end
	assert(
		origin == "filesystem" or origin == "studio",
		"Studio-only sync received a publication without valid provenance. Drafts were preserved; reconnect to a compatible server."
	)
	return origin == "studio"
end

function DirectionPolicy.resumeKey(endpoint, sessionId, direction)
	assert(
		type(endpoint) == "string" and type(sessionId) == "string" and DirectionPolicy.isDirection(direction),
		"A resumable session requires an exact endpoint, session identity, and direction."
	)
	return `{#endpoint}:{endpoint}{#sessionId}:{sessionId}{direction}`
end

return DirectionPolicy
