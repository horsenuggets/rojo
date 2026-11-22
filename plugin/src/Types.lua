local Packages = script.Parent.Parent.Packages
local t = require(Packages.t)
local Settings = require(script.Parent.Settings)
local strict = require(script.Parent.strict)

local RbxId = t.string

local ApiValue = t.keys(t.string)

local ApiInstanceMetadata = t.interface({
	ignoreUnknownInstances = t.optional(t.boolean),
})

local ApiInstance = t.interface({
	Id = RbxId,
	Parent = t.optional(RbxId),
	Name = t.string,
	ClassName = t.string,
	Properties = t.map(t.string, ApiValue),
	Metadata = t.optional(ApiInstanceMetadata),
	Children = t.array(RbxId),
})

local ApiInstanceUpdate = t.interface({
	id = RbxId,
	changedName = t.optional(t.string),
	changedParent = t.optional(RbxId),
	changedClassName = t.optional(t.string),
	changedProperties = t.map(t.string, ApiValue),
	changedMetadata = t.optional(ApiInstanceMetadata),
})

local ApiSubscribeMessage = t.interface({
	removed = t.array(RbxId),
	added = t.map(RbxId, ApiInstance),
	updated = t.array(ApiInstanceUpdate),
	sourceOperations = t.optional(t.map(RbxId, t.string)),
})

local ApiInfoResponse = t.interface({
	sessionId = t.string,
	serverVersion = t.string,
	protocolVersion = t.number,
	expectedPlaceIds = t.optional(t.array(t.number)),
	rootInstanceId = RbxId,
})

local ApiReadResponse = t.interface({
	sessionId = t.string,
	messageCursor = t.number,
	instances = t.map(RbxId, ApiInstance),
})

-- Acknowledgements contain Rust patch snapshots, not subscription instance records.
local ApiWriteMetadata = t.interface({
	ignore_unknown_instances = t.boolean,
	relevant_paths = t.array(t.string),
	context = t.interface({ emit_legacy_scripts = t.boolean }),
	specified_id = t.optional(t.string),
	middleware = t.optional(t.string),
	schema = t.optional(t.string),
	installed_paths = t.optional(t.array(t.string)),
})

local ApiWriteSnapshot
ApiWriteSnapshot = t.interface({
	snapshot_id = RbxId,
	metadata = ApiWriteMetadata,
	name = t.string,
	class_name = t.string,
	properties = t.map(t.string, ApiValue),
	children = t.array(function(child)
		return ApiWriteSnapshot(child)
	end),
})

local ApiWriteUpdate = t.interface({
	id = RbxId,
	changed_name = t.optional(t.string),
	changed_class_name = t.optional(t.string),
	changedParent = t.optional(RbxId),
	changed_properties = t.map(t.string, ApiValue),
	changed_metadata = t.optional(ApiWriteMetadata),
})

local ApiWritePatch = t.interface({
	removed_instances = t.array(RbxId),
	added_instances = t.array(t.interface({ parent_id = RbxId, instance = ApiWriteSnapshot })),
	updated_instances = t.array(ApiWriteUpdate),
})

local ApiWriteError = t.interface({
	id = RbxId,
	kind = t.optional(t.literal("sourceSerializationDeferred")),
	property = t.optional(t.string),
	message = t.string,
})

local function acknowledgementToken(value)
	return type(value) == "string"
		and value:match("^%x%x%x%x%x%x%x%x%-%x%x%x%x%-4%x%x%x%-[89aAbB]%x%x%x%-%x%x%x%x%x%x%x%x%x%x%x%x$") ~= nil
end

local ApiWriteResponse = t.interface({
	acknowledgement = t.optional(acknowledgementToken),
	sessionId = t.string,
	operationId = t.optional(t.string),
	messageCursor = t.intersection(t.integer, t.numberConstrained(0, 4294967295)),
	accepted = ApiWritePatch,
	conflicts = ApiWritePatch,
	errors = t.array(ApiWriteError),
})

local SocketPacketType = t.union(t.literal("messages"), t.literal("export"))

local ExportPacket = t.interface({
	jobId = t.string,
	scope = t.union(t.literal("place"), t.literal("model")),
	paths = t.array(t.string),
	selection = t.boolean,
	allowIncomplete = t.boolean,
	partSize = t.literal(1048576),
})

local MessagesPacket = t.interface({
	messageCursor = t.number,
	messages = t.array(ApiSubscribeMessage),
})

local SocketPacketBody = t.union(MessagesPacket, ExportPacket)

local ApiSocketPacket = t.interface({
	sessionId = t.string,
	packetType = SocketPacketType,
	body = SocketPacketBody,
})

local ApiSerializeResponse = t.interface({
	sessionId = t.string,
	modelContents = t.buffer,
})

local ApiRefPatchResponse = t.interface({
	sessionId = t.string,
	patch = ApiSubscribeMessage,
})

local ApiError = t.interface({
	kind = t.union(t.literal("NotFound"), t.literal("BadRequest"), t.literal("InternalError")),
	details = t.string,
})

local function ifEnabled(innerCheck)
	return function(...)
		if Settings:get("typecheckingEnabled") then
			return innerCheck(...)
		else
			return true
		end
	end
end

return strict("Types", {
	ifEnabled = ifEnabled,

	ApiInfoResponse = ApiInfoResponse,
	ApiReadResponse = ApiReadResponse,
	ApiWriteResponse = ApiWriteResponse,
	ApiSocketPacket = ApiSocketPacket,
	ExportPacket = ExportPacket,
	ApiError = ApiError,

	ApiInstance = ApiInstance,
	ApiInstanceUpdate = ApiInstanceUpdate,
	ApiInstanceMetadata = ApiInstanceMetadata,
	ApiSubscribeMessage = ApiSubscribeMessage,
	ApiSerializeResponse = ApiSerializeResponse,
	ApiRefPatchResponse = ApiRefPatchResponse,
	ApiValue = ApiValue,
	RbxId = RbxId,

	-- Deprecated aliases during transition
	VirtualInstance = ApiInstance,
	VirtualMetadata = ApiInstanceMetadata,
	VirtualValue = ApiValue,
})
