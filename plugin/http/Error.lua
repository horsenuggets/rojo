local msgpack = require(script.Parent.Parent.msgpack)

local Error = {}
Error.__index = Error

Error.Kind = {
	HttpNotEnabled = {
		message = "Rojo requires HTTP access, which is not enabled.\n"
			.. "Check your game settings, located in the 'Home' tab of Studio.",
	},
	ConnectFailed = {
		message = "Couldn't connect to the Rojo+ server.\n"
			.. "Make sure the server is running — use 'rojo serve' to run it!",
	},
	Protocol = {
		message = "Invalid Rojo+ server response. {{message}}",
	},
	Timeout = {
		message = "HTTP request timed out.",
	},
	Unknown = {
		message = "Unknown HTTP error: {{message}}",
	},
}

setmetatable(Error.Kind, {
	__index = function(_, key)
		error(("%q is not a valid member of Http.Error.Kind"):format(tostring(key)), 2)
	end,
})

function Error.new(type, extraMessage)
	extraMessage = extraMessage or ""
	-- A function replacement keeps "%" in the message from being read as a
	-- capture reference.
	local message = type.message:gsub("{{message}}", function()
		return extraMessage
	end)

	local err = {
		type = type,
		message = message,
	}

	setmetatable(err, Error)

	return err
end

function Error:__tostring()
	return self.message
end

--[[
	This method shouldn't have to exist. Ugh.
]]
function Error.fromRobloxErrorString(message)
	local lower = message:lower()

	if lower:find("^http requests are not enabled") then
		return Error.new(Error.Kind.HttpNotEnabled)
	end

	if lower:find("^httperror: timedout") then
		return Error.new(Error.Kind.Timeout)
	end

	if lower:find("^httperror: connectfail") then
		return Error.new(Error.Kind.ConnectFailed)
	end

	return Error.new(Error.Kind.Unknown, message)
end

function Error.fromResponse(response)
	local lower = (response.body or ""):lower()
	if response.code == 408 or response.code == 504 or lower:find("timed? ?out") then
		return Error.new(Error.Kind.Timeout)
	end

	-- Rojo servers send errors as MessagePack with a readable `details` field,
	-- which is shown instead of the raw bytes when it's there.
	local details = Error.detailsFromBody(response.body)
	local err = Error.new(
		Error.Kind.Unknown,
		string.format("%s: %s", tostring(response.code), details or tostring(response.body))
	)
	err.details = details
	err.response = response
	return err
end

--[[
	Returns the `details` of a Rojo error body, or nil when the body isn't one.
]]
function Error.detailsFromBody(body)
	if type(body) ~= "string" or body == "" then
		return nil
	end
	local ok, decoded = pcall(msgpack.decode, body)
	if ok and type(decoded) == "table" and type(decoded.details) == "string" then
		return decoded.details
	end
	return nil
end

return Error
