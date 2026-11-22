local Packages = script.Parent.Parent.Packages
local HttpService = game:GetService("HttpService")
local Http = require(Packages.Http)
local Log = require(Packages.Log)
local Promise = require(Packages.Promise)

local Config = require(script.Parent.Config)
local Types = require(script.Parent.Types)
local Version = require(script.Parent.Version)

local validateApiInfo = Types.ifEnabled(Types.ApiInfoResponse)
local validateApiRead = Types.ifEnabled(Types.ApiReadResponse)
local validateApiSocketPacket = Types.ifEnabled(Types.ApiSocketPacket)
local validateApiSerialize = Types.ifEnabled(Types.ApiSerializeResponse)
local validateApiRefPatch = Types.ifEnabled(Types.ApiRefPatchResponse)
-- Acknowledgement ordering is a wire contract, not an optional debug check.
local validateApiWrite = Types.ApiWriteResponse

local function rejectFailedRequests(response)
	if response.code >= 400 then
		local message = string.format("HTTP %s:\n%s", tostring(response.code), response.body)

		return Promise.reject(message)
	end

	return response
end

local function rejectInvalidAcknowledgement(context, message)
	context:disconnect()
	return Promise.reject(Http.Error.new(Http.Error.Kind.Protocol, message))
end

-- Servers from before the switch to msgpack, including upstream Rojo, answer the info
-- request with JSON. Decoding JSON as msgpack turns its leading "{" into the number 123,
-- so fall back to JSON whenever msgpack doesn't produce a table. That way an old server
-- reaches the protocol version check below and gets a clear message.
local function decodeInfoResponse(response)
	local msgpackOk, msgpackBody = pcall(Http.Response.msgpack, response)
	if msgpackOk and type(msgpackBody) == "table" then
		return msgpackBody
	end

	local jsonOk, jsonBody = pcall(Http.Response.json, response)
	if jsonOk and type(jsonBody) == "table" then
		return jsonBody
	end

	return Promise.reject(
		"Found a server at this address, but couldn't read its response."
			.. "\nMake sure the address points at a Rojo+ server."
	)
end

local function rejectWrongProtocolVersion(infoResponseBody)
	if infoResponseBody.protocolVersion ~= Config.protocolVersion then
		local message = (
			"Found a Rojo+ dev server, but it's using a different protocol version, and is incompatible."
			.. "\nMake sure you have matching versions of both the Rojo+ plugin and server!"
			.. "\n\nYour client is version %s, with protocol version %s. It expects server version %s."
			.. "\nYour server is version %s, with protocol version %s."
			.. "\n\nGo to https://github.com/rojo-rbx/rojo for more details."
		):format(
			Version.display(Config.version),
			Config.protocolVersion,
			Config.expectedServerVersionString,
			infoResponseBody.serverVersion,
			infoResponseBody.protocolVersion
		)

		return Promise.reject(message)
	end

	return Promise.resolve(infoResponseBody)
end

local function rejectWrongPlaceId(infoResponseBody)
	if infoResponseBody.expectedPlaceIds ~= nil then
		local foundId = table.find(infoResponseBody.expectedPlaceIds, game.PlaceId)

		if not foundId then
			local idList = {}
			for _, id in ipairs(infoResponseBody.expectedPlaceIds) do
				table.insert(idList, "- " .. tostring(id))
			end

			local message = (
				"Found a Rojo+ server, but its project is set to only be used with a specific list of places."
				.. "\nYour place ID is %u, but needs to be one of these:"
				.. "\n%s"
				.. "\n\nTo change this list, edit 'servePlaceIds' in your .project.json file."
			):format(game.PlaceId, table.concat(idList, "\n"))

			return Promise.reject(message)
		end
	end

	if infoResponseBody.unexpectedPlaceIds ~= nil then
		local foundId = table.find(infoResponseBody.unexpectedPlaceIds, game.PlaceId)

		if foundId then
			local idList = {}
			for _, id in ipairs(infoResponseBody.unexpectedPlaceIds) do
				table.insert(idList, "- " .. tostring(id))
			end

			local message = (
				"Found a Rojo+ server, but its project is set to not be used with a specific list of places."
				.. "\nYour place ID is %u, but needs to not be one of these:"
				.. "\n%s"
				.. "\n\nTo change this list, edit 'blockedPlaceIds' in your .project.json file."
			):format(game.PlaceId, table.concat(idList, "\n"))

			return Promise.reject(message)
		end
	end

	return Promise.resolve(infoResponseBody)
end

local ApiContext = {}
ApiContext.__index = ApiContext

function ApiContext.new(baseUrl)
	assert(type(baseUrl) == "string", "baseUrl must be a string")

	local self = {
		__baseUrl = baseUrl,
		__sessionId = nil,
		__messageCursor = -1,
		__writeCursor = 0,
		__wsClient = nil,
		__connected = true,
		__activeRequests = {},
	}

	return setmetatable(self, ApiContext)
end

function ApiContext:__fmtDebug(output)
	output:writeLine("ApiContext {{")
	output:indent()

	output:writeLine("Connected: {}", self.__connected)
	output:writeLine("Base URL: {}", self.__baseUrl)
	output:writeLine("Session ID: {}", self.__sessionId)
	output:writeLine("Message Cursor: {}", self.__messageCursor)

	output:unindent()
	output:write("}")
end

function ApiContext:disconnect()
	self.__connected = false
	for request in self.__activeRequests do
		Log.trace("Cancelling request {}", request)
		request:cancel()
	end
	self.__activeRequests = {}

	if self.__wsClient then
		Log.trace("Closing WebSocket client")
		self.__wsClient:Close()
	end
	self.__wsClient = nil
end

function ApiContext:getBaseUrl()
	return self.__baseUrl
end

function ApiContext:getMessageCursor()
	return self.__messageCursor
end

function ApiContext:setMessageCursor(index)
	self.__messageCursor = index
end

function ApiContext:connect()
	local url = ("%s/api/rojo"):format(self.__baseUrl)

	return Http.get(url)
		:andThen(rejectFailedRequests)
		:andThen(decodeInfoResponse)
		:andThen(rejectWrongProtocolVersion)
		:andThen(function(body)
			assert(validateApiInfo(body))

			return body
		end)
		:andThen(rejectWrongPlaceId)
		:andThen(function(body)
			self.__sessionId = body.sessionId
			self.__writeCursor = 0

			return body
		end)
end

function ApiContext:read(ids)
	local url = ("%s/api/read/%s"):format(self.__baseUrl, table.concat(ids, ","))

	return Http.get(url):andThen(rejectFailedRequests):andThen(Http.Response.msgpack):andThen(function(body)
		if body.sessionId ~= self.__sessionId then
			return Promise.reject("Server changed ID")
		end

		assert(validateApiRead(body))

		return body
	end)
end

function ApiContext:write(patch, messageCursor, authority, operationId, predecessor, sourceBaselines)
	assert(authority == "Filesystem" or authority == "Studio", "Write authority must be Filesystem or Studio")
	assert(predecessor == nil or type(predecessor) == "string", "Write predecessor must be an opaque string.")
	assert(sourceBaselines == nil or type(sourceBaselines) == "table", "Source baselines must be a cursor map.")
	for id, cursor in sourceBaselines or {} do
		assert(
			type(id) == "string"
				and type(cursor) == "number"
				and cursor % 1 == 0
				and cursor >= 0
				and cursor <= 4294967295,
			"Source baselines must contain script identities and u32 cursors."
		)
	end
	if not self.__connected then
		return Promise.reject("The connection is closed.")
	end
	local url = ("%s/api/write"):format(self.__baseUrl)

	local updated = {}
	for _, update in ipairs(patch.updated) do
		local fixedUpdate = {
			id = update.id,
			changedName = update.changedName,
			changedParent = update.changedParent,
		}

		if next(update.changedProperties) ~= nil then
			fixedUpdate.changedProperties = update.changedProperties
		end

		table.insert(updated, fixedUpdate)
	end

	-- Only add the 'added' field if the table is non-empty, or else the msgpack
	-- encode implementation will turn the table into an array instead of a map,
	-- causing API validation to fail.
	local added
	if next(patch.added) ~= nil then
		added = patch.added
	end

	local body = {
		sessionId = self.__sessionId,
		operationId = operationId,
		predecessor = predecessor,
		-- Empty tables encode as arrays, not maps, in the wire encoder.
		sourceBaselines = if sourceBaselines and next(sourceBaselines) ~= nil then sourceBaselines else nil,
		messageCursor = messageCursor or self.__messageCursor,
		authority = authority,
		removed = patch.removed,
		updated = updated,
		added = added,
	}

	-- Subscription progress may advance while HTTP is in flight. Only progress
	-- already known at dispatch constrains this response's publication cursor.
	local minimumCursor = math.max(body.messageCursor, self.__messageCursor, self.__writeCursor)
	body = Http.msgpackEncode(body)

	return Http.post(url, body):andThen(rejectFailedRequests):andThen(function(response)
		local decoded, responseBody = pcall(Http.Response.msgpack, response)
		if not decoded then
			return rejectInvalidAcknowledgement(self, "The write acknowledgement could not be decoded.")
		end
		if not validateApiWrite(responseBody) then
			return rejectInvalidAcknowledgement(self, "The write acknowledgement is incomplete or has no valid cursor.")
		end
		if responseBody.sessionId ~= self.__sessionId then
			return rejectInvalidAcknowledgement(self, "Server changed ID")
		end
		if operationId ~= nil and responseBody.operationId ~= operationId then
			return rejectInvalidAcknowledgement(self, "The write acknowledgement belongs to another operation.")
		end
		if responseBody.messageCursor < minimumCursor then
			return rejectInvalidAcknowledgement(self, "The write acknowledgement predates known publication progress.")
		end
		local conflicts = responseBody.conflicts
		if
			responseBody.acknowledgement ~= nil
			and (
				#responseBody.errors > 0
				or #conflicts.removed_instances > 0
				or #conflicts.added_instances > 0
				or #conflicts.updated_instances > 0
			)
		then
			return rejectInvalidAcknowledgement(self, "A rejected write cannot grant a lineage capability.")
		end
		self.__writeCursor = math.max(self.__writeCursor, responseBody.messageCursor)
		return responseBody
	end)
end

function ApiContext:__sourceCheckpointRequest(method, checkpointId, body)
	if not self.__connected then
		return Promise.reject("The connection is closed.")
	end
	local url = self.__baseUrl .. "/api/source-checkpoints"
	if checkpointId ~= nil then
		assert(
			type(checkpointId) == "string" and #checkpointId <= 96 and checkpointId:match("^[%w_%-]+$") ~= nil,
			"Checkpoint identity must be an opaque identifier."
		)
		url ..= "/" .. checkpointId
	end
	local request
	if method == "GET" then
		request = Http.get(url)
	else
		body = table.clone(body)
		body.sessionId = self.__sessionId
		request = if method == "POST"
			then Http.post(url, Http.msgpackEncode(body))
			else Http.delete(url, Http.msgpackEncode(body))
	end
	return request:andThen(rejectFailedRequests):andThen(Http.Response.msgpack):andThen(function(response)
		if type(response) ~= "table" or response.sessionId ~= self.__sessionId then
			return rejectInvalidAcknowledgement(self, "The Source checkpoint response has an invalid session.")
		end
		return response
	end)
end

function ApiContext:createSourceCheckpoint(id, source, expectedSource, messageCursor)
	assert(
		type(id) == "string" and type(source) == "string" and type(expectedSource) == "string",
		"Source checkpoints require script identity and text."
	)
	return self:__sourceCheckpointRequest("POST", nil, {
		id = id,
		source = source,
		expectedSource = expectedSource,
		messageCursor = messageCursor,
	}):andThen(function(response)
		if type(response.checkpointId) ~= "string" or #response.checkpointId == 0 then
			return rejectInvalidAcknowledgement(self, "The Source checkpoint was not acknowledged as durable.")
		end
		return response
	end)
end

function ApiContext:listSourceCheckpoints()
	return self:__sourceCheckpointRequest("GET")
end

function ApiContext:readSourceCheckpoint(checkpointId)
	return self:__sourceCheckpointRequest("GET", checkpointId)
end

function ApiContext:discardSourceCheckpoint(checkpointId)
	return self:__sourceCheckpointRequest("DELETE", checkpointId, {})
end

function ApiContext:connectWebSocket(packetHandlers)
	local url = ("%s/api/socket/%s"):format(self.__baseUrl, self.__messageCursor)
	-- Convert HTTP/HTTPS URL to WS/WSS
	url = url:gsub("^http://", "ws://"):gsub("^https://", "wss://")

	return Promise.new(function(resolve, reject)
		local success, wsClient =
			pcall(HttpService.CreateWebStreamClient, HttpService, Enum.WebStreamClientType.WebSocket, {
				Url = url,
			})
		if not success then
			reject("Failed to create WebSocket client: " .. tostring(wsClient))
			return
		end
		self.__wsClient = wsClient

		local closed, errored, received

		received = self.__wsClient.MessageReceived:Connect(function(msg)
			if not self.__connected then
				return
			end
			local data = Http.msgpackDecode(msg)
			if data.sessionId ~= self.__sessionId then
				Log.warn("Received message with wrong session ID; ignoring")
				return
			end

			assert(validateApiSocketPacket(data))

			Log.trace("Received websocket packet: {:#?}", data)

			local handler = packetHandlers[data.packetType]
			if handler then
				local ok, err = pcall(handler, data.body)
				if not ok then
					Log.error("Error in WebSocket packet handler for type '%s': %s", data.packetType, err)
				end
			else
				Log.warn("No handler for WebSocket packet type '%s'", data.packetType)
			end
		end)

		closed = self.__wsClient.Closed:Connect(function()
			closed:Disconnect()
			errored:Disconnect()
			received:Disconnect()

			if self.__connected then
				reject("WebSocket connection closed unexpectedly")
			else
				resolve()
			end
		end)

		errored = self.__wsClient.Error:Connect(function(code, msg)
			closed:Disconnect()
			errored:Disconnect()
			received:Disconnect()

			reject("WebSocket error: " .. code .. " - " .. msg)
		end)
	end)
end

function ApiContext:open(id)
	local url = ("%s/api/open/%s"):format(self.__baseUrl, id)

	return Http.post(url, ""):andThen(rejectFailedRequests):andThen(Http.Response.msgpack):andThen(function(body)
		if body.sessionId ~= self.__sessionId then
			return Promise.reject("Server changed ID")
		end

		return nil
	end)
end

function ApiContext:serialize(ids: { string })
	local url = ("%s/api/serialize"):format(self.__baseUrl)
	local request_body = Http.msgpackEncode({ sessionId = self.__sessionId, ids = ids })

	return Http.post(url, request_body)
		:andThen(rejectFailedRequests)
		:andThen(Http.Response.msgpack)
		:andThen(function(response_body)
			if response_body.sessionId ~= self.__sessionId then
				return Promise.reject("Server changed ID")
			end

			assert(validateApiSerialize(response_body))

			return response_body
		end)
end

function ApiContext:refPatch(ids: { string })
	local url = ("%s/api/ref-patch"):format(self.__baseUrl)
	local request_body = Http.msgpackEncode({ sessionId = self.__sessionId, ids = ids })

	return Http.post(url, request_body)
		:andThen(rejectFailedRequests)
		:andThen(Http.Response.msgpack)
		:andThen(function(response_body)
			if response_body.sessionId ~= self.__sessionId then
				return Promise.reject("Server changed ID")
			end

			assert(validateApiRefPatch(response_body))

			return response_body
		end)
end

return ApiContext
