local Packages = script.Parent.Parent.Packages
local HttpService = game:GetService("HttpService")
local Http = require(Packages.Http)
local Log = require(Packages.Log)
local Promise = require(Packages.Promise)

local Config = require(script.Parent.Config)
local DirectionPolicy = require(script.Parent.DirectionPolicy)
local Types = require(script.Parent.Types)
local Version = require(script.Parent.Version)
local yieldBudget = require(script.Parent.yieldBudget)

local GET_MAX_ATTEMPTS = 3
local GET_RETRY_DELAY = 0.25
local CONNECTION_CLOSED = "The connection is closed."
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

local function rejectWrongProtocolVersion(infoResponseBody, expectedProtocolVersion)
	if infoResponseBody.protocolVersion ~= expectedProtocolVersion then
		local message = (
			"Found a Rojo+ dev server, but it's using a different protocol version, and is incompatible."
			.. "\nMake sure you have matching versions of both the Rojo+ plugin and server!"
			.. "\n\nYour client is version %s, with protocol version %s. It expects server version %s."
			.. "\nYour server is version %s, with protocol version %s."
			.. "\n\nGo to https://github.com/rojo-rbx/rojo for more details."
		):format(
			Version.display(Config.version),
			expectedProtocolVersion,
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
		__generation = 0,
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

function ApiContext:__cancelRequests()
	local requests = self.__activeRequests
	self.__activeRequests = {}
	for request in requests do
		Log.trace("Cancelling request {}", request)
		request:cancel()
	end
end

function ApiContext:__trackRequest(request)
	self.__activeRequests[request] = true
	request
		:finally(function()
			self.__activeRequests[request] = nil
		end)
		:catch(function() end)
	return request
end

function ApiContext:__isCurrent(generation)
	return self.__connected and self.__generation == generation
end

-- Only transport failures without an HTTP response are retried. Decoding and protocol
-- checks run once, after a successful GET. Track both the transport/delay and the returned operation because
-- cancelling a consumer alone does not necessarily cancel a shared Promise producer.
function ApiContext:__get(endpoint, phase, processResponse)
	if not self.__connected then
		return Promise.reject(CONNECTION_CLOSED)
	end
	local generation = self.__generation
	local attempts = 0
	local function attempt()
		if not self:__isCurrent(generation) then
			return Promise.reject(CONNECTION_CLOSED)
		end
		attempts += 1
		return self:__trackRequest(Http.get(self.__baseUrl .. endpoint)):catch(function(err)
			if not self:__isCurrent(generation) then
				return Promise.reject(CONNECTION_CLOSED)
			end
			if
				attempts < GET_MAX_ATTEMPTS
				and getmetatable(err) == Http.Error
				and err.response == nil
				and (err.type == Http.Error.Kind.Timeout or err.type == Http.Error.Kind.ConnectFailed)
			then
				return self:__trackRequest(Promise.delay(GET_RETRY_DELAY * attempts)):andThen(attempt)
			end
			return Promise.reject(err)
		end)
	end
	local request = attempt()
		:andThen(function(response)
			if not self:__isCurrent(generation) then
				return Promise.reject(CONNECTION_CLOSED)
			end
			if response.code >= 400 then
				-- Never dump an endpoint's response body into startup diagnostics.
				return Promise.reject(("HTTP %s."):format(tostring(response.code)))
			end
			return processResponse(response, generation)
		end)
		:andThen(function(body)
			if not self:__isCurrent(generation) then
				return Promise.reject(CONNECTION_CLOSED)
			end
			return body
		end)
		:catch(function(err)
			local diagnostic = ("%s GET %s failed after %d %s."):format(
				phase,
				endpoint:match("^/api/[^/]+"),
				attempts,
				if attempts == 1 then "attempt" else "attempts"
			)
			if getmetatable(err) == Http.Error then
				local contextual = setmetatable(table.clone(err), Http.Error)
				local detail = if err.type == Http.Error.Kind.Timeout
						or err.type == Http.Error.Kind.ConnectFailed
						or err.type == Http.Error.Kind.HttpNotEnabled
					then tostring(err)
					else "The HTTP request failed."
				contextual.message = diagnostic .. " " .. detail
				return Promise.reject(contextual)
			end
			return Promise.reject(diagnostic .. " " .. tostring(err))
		end)
	return self:__trackRequest(request)
end

function ApiContext:disconnect()
	self.__connected = false
	self:__cancelRequests()

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
	if not self.__connected then
		return Promise.reject(CONNECTION_CLOSED)
	end
	self.__generation += 1
	self:__cancelRequests()
	return self:__get("/api/rojo", "Server info", function(response, generation)
		return Promise.resolve(response)
			:andThen(decodeInfoResponse)
			:andThen(function(body)
				local expectedProtocolVersion = if body.syncDirection ~= nil
					then Config.directionalProtocolVersion
					else Config.protocolVersion
				return rejectWrongProtocolVersion(body, expectedProtocolVersion):catch(function(problem)
					if body.syncDirection ~= nil and self:__isCurrent(generation) then
						self.__directionError = "Directional sync negotiation failed. " .. tostring(problem)
					end
					return Promise.reject(problem)
				end)
			end)
			:andThen(function(body)
				local valid, problem = pcall(DirectionPolicy.validateServer, body)
				if not valid then
					self.__directionError = "Directional sync negotiation failed. " .. tostring(problem)
					return Promise.reject(self.__directionError)
				end
				assert(validateApiInfo(body))
				return body
			end)
			:andThen(rejectWrongPlaceId)
			:andThen(function(body)
				if not self:__isCurrent(generation) then
					return Promise.reject(CONNECTION_CLOSED)
				end
				self.__sessionId = body.sessionId
				self.__syncDirection = body.syncDirection
				self.__directionError = nil
				self.__writeCursor = 0
				return body
			end)
	end)
end

function ApiContext:read(ids)
	local endpoint = ("/api/read/%s"):format(table.concat(ids, ","))
	local cancelled = false
	local request
	return self:__trackRequest(Promise.new(function(resolve, _, onCancel)
		onCancel(function()
			cancelled = true
			if request then
				request:cancel()
			end
		end)
		request = self:__get(endpoint, "Instance read", function(response, generation)
			local step = yieldBudget()
			local body = Http.Response.msgpack(response, function()
				-- Promise continuations can outlive their cancelled executor. Fence both sides
				-- of a yield with connection and per-read ownership before parsing more tokens.
				assert(self:__isCurrent(generation), CONNECTION_CLOSED)
				assert(not cancelled, "The instance read was cancelled.")
				step()
				assert(self:__isCurrent(generation), CONNECTION_CLOSED)
				assert(not cancelled, "The instance read was cancelled.")
			end)
			if body.sessionId ~= self.__sessionId then
				return Promise.reject("Server changed ID")
			end
			assert(validateApiRead(body))
			return body
		end)
		resolve(request)
	end))
end

function ApiContext:write(patch, messageCursor, authority, operationId, predecessor, sourceBaselines)
	if self.__directionError ~= nil then
		return Promise.reject(self.__directionError)
	end
	if self.__syncDirection == "fs-to-studio" then
		return Promise.reject("The server policy prohibits Studio writes in filesystem-only sync.")
	end
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
	if self.__directionError ~= nil and method ~= "GET" then
		return Promise.reject(self.__directionError)
	end
	if self.__syncDirection == "fs-to-studio" and method ~= "GET" then
		return Promise.reject("The filesystem-only policy prohibits modifying Source recovery checkpoints.")
	end
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

-- Kept as an I/O boundary so protocol rejection can be tested without native sockets.
function ApiContext:__createWebStreamClient(url)
	return HttpService:CreateWebStreamClient(Enum.WebStreamClientType.WebSocket, { Url = url })
end

function ApiContext:connectWebSocket(packetHandlers)
	local url = ("%s/api/socket/%s"):format(self.__baseUrl, self.__messageCursor)
	-- Convert HTTP/HTTPS URL to WS/WSS
	url = url:gsub("^http://", "ws://"):gsub("^https://", "wss://")

	return Promise.new(function(resolve, reject)
		local success, wsClient = pcall(self.__createWebStreamClient, self, url)
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

			if not validateApiSocketPacket(data) then
				reject(Http.Error.new(Http.Error.Kind.Protocol, "The server sent an invalid publication packet."))
				self:disconnect()
				return
			end

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
