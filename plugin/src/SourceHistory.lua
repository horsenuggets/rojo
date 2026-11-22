--[[
	SourceHistory

	Bounded causal checkpoints for Source writes. Matching text or a recent timestamp never
	proves an echo. Acknowledged operations remain pinned until publication coverage is
	consumed. Ambiguous transport failures remain unresolved instead of silently aging out.
]]

const SourceHistory = {}
SourceHistory.__index = SourceHistory

function SourceHistory.new(options)
	assert(type(options.Clock) == "function", "A history clock is required.")
	assert(options.Horizon >= 0 and options.MaxEntries > 0 and options.MaxBytes > 0, "History bounds must be valid.")
	return setmetatable({
		Bytes = 0,
		Clock = options.Clock,
		ConsumedCursor = 0,
		Entries = {},
		Horizon = options.Horizon,
		MaxBytes = options.MaxBytes,
		MaxEntries = options.MaxEntries,
		Order = {},
	}, SourceHistory)
end

function SourceHistory:__remove(operation)
	const entry = self.Entries[operation]
	self.Bytes -= entry.Bytes
	self.Entries[operation] = nil
end

function SourceHistory:__prune(requiredBytes, requiredEntries)
	const now = self.Clock()
	local retained = {}
	for _, operation in self.Order do
		const entry = self.Entries[operation]
		if entry.Completed ~= nil and now - entry.Completed > self.Horizon then
			self:__remove(operation)
		else
			table.insert(retained, operation)
		end
	end
	self.Order = retained
	if self.Bytes + requiredBytes <= self.MaxBytes and #retained + requiredEntries <= self.MaxEntries then
		return
	end
	retained = {}
	local count = #self.Order
	for _, operation in self.Order do
		const entry = self.Entries[operation]
		if
			entry.Completed ~= nil
			and (self.Bytes + requiredBytes > self.MaxBytes or count + requiredEntries > self.MaxEntries)
		then
			self:__remove(operation)
			count -= 1
		else
			table.insert(retained, operation)
		end
	end
	self.Order = retained
end

function SourceHistory:begin(operation, sources)
	assert(type(operation) == "string" and self.Entries[operation] == nil, "Write operation identity must be unique.")
	local bytes = #operation
	const entries = {}
	for id, source in sources do
		assert(type(id) == "string" and type(source.Source) == "string", "Source checkpoints must contain text.")
		bytes += #id + #source.Source
		entries[id] = table.clone(source)
	end
	assert(
		bytes <= self.MaxBytes,
		"This Source checkpoint exceeds the history byte capacity. Existing checkpoints were preserved."
	)
	self:__prune(bytes, 1)
	assert(
		self.Bytes + bytes <= self.MaxBytes and #self.Order < self.MaxEntries,
		"Source checkpoint capacity is exhausted. Pending edits have been preserved."
	)
	self.Bytes += bytes
	self.Entries[operation] = { Bytes = bytes, Sources = entries }
	table.insert(self.Order, operation)
end

function SourceHistory:finish(operation)
	const entry = self.Entries[operation]
	if entry ~= nil and entry.Completed == nil then
		entry.Completed = self.Clock()
	end
end

function SourceHistory:acknowledge(operation, publicationCursor)
	assert(type(publicationCursor) == "number" and publicationCursor >= 0, "Publication coverage must be valid.")
	const entry = self.Entries[operation]
	if entry ~= nil then
		entry.CoverageCursor = publicationCursor
		if publicationCursor <= self.ConsumedCursor then
			self:finish(operation)
		end
	end
end

function SourceHistory:consume(publicationCursor)
	assert(type(publicationCursor) == "number" and publicationCursor >= 0, "Consumed coverage must be valid.")
	self.ConsumedCursor = math.max(self.ConsumedCursor, publicationCursor)
	for operation, entry in self.Entries do
		if entry.CoverageCursor ~= nil and entry.CoverageCursor <= self.ConsumedCursor then
			self:finish(operation)
		end
	end
end

function SourceHistory:classify(id, operation, instance, revision)
	self:__prune(0, 0)
	const entry = if operation ~= nil then self.Entries[operation] else nil
	const source = if entry ~= nil then entry.Sources[id] else nil
	if source == nil or source.Instance ~= instance or revision < source.Revision then
		return "Independent"
	end
	return if revision > source.Revision then "Echo" else "Current"
end

return SourceHistory
