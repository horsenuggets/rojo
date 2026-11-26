local RunService = game:GetService("RunService")

local Packages = script.Parent.Parent.Packages
local Log = require(Packages.Log)
local Source = require(script.Parent.Reconciler.setSource)
local UnmanagedAttributes = require(script.Parent.Reconciler.unmanagedAttributes)

--[[
	A bidirectional map between instance IDs and Roblox instances. It lets us
	keep track of every instance we know about.
]]
local InstanceMap = {}
InstanceMap.__index = InstanceMap

function InstanceMap.new(onInstanceChanged, onChildAdded, sourceSetter)
	local self = {
		-- Shared with the reconciler, but replaceable with a deterministic editor in tests.
		sourceSetter = sourceSetter or Source.shared(),
		sourceConnections = {},

		-- A map from IDs to instances.
		fromIds = {},

		-- A map from instances to IDs.
		fromInstances = {},

		-- Last authoritative parents, not speculative Studio moves awaiting a reply.
		knownParents = {},

		-- Structural notifications can be deferred until after a later local edit.
		observedNames = {},
		observedParents = {},
		structuralAssignments = {},

		-- A set of all instances that updates should be paused for. This set
		-- should generally be empty, and will be filled by pauseInstance
		-- temporarily.
		pausedUpdateInstances = {},

		-- A map from instances to a signal or list of signals connected to it.
		instancesToSignal = {},

		-- Source includes an observation as the third argument and bypasses the generic
		-- pause. Its Operation is only a candidate until onSourceOutcome confirms it.
		onInstanceChanged = onInstanceChanged,

		-- Callback that's invoked with each instance that gains a known parent
		-- while it isn't known itself, which is how Studio-made instances appear.
		onChildAdded = onChildAdded,
	}

	return setmetatable(self, InstanceMap)
end

function InstanceMap:size()
	local size = 0

	for _ in pairs(self.fromIds) do
		size = size + 1
	end

	return size
end

--[[
	Disconnect all connections and release all instance references.
]]
function InstanceMap:stop()
	-- I think this is safe.
	for instance in pairs(self.fromInstances) do
		self:removeInstance(instance, "stopped")
	end
end

-- Returns the live observation revision and its instance/session generation.
function InstanceMap:getSourceRevision(instance)
	local revision, generation = self.sourceSetter.GetRevision(instance)
	local connection = self.sourceConnections[instance]
	if connection and connection.Generation == generation then
		return revision, generation
	end
	return nil, nil
end

-- The returned operation is pending until the editor reports its actual outcome.
-- onSourceScheduled(instance, operation) runs before starting the asynchronous work.
-- onSourceOutcome(instance, operation) runs once at completion, including cancellation.
-- Both callbacks also cover pending writes adopted during insertion. ServerOperation is
-- the optional protocol token, not a content-based echo identity.
function InstanceMap:setSource(instance, value, completed, serverOperation)
	assert(self.fromInstances[instance] ~= nil, "Source instance must be tracked.")
	assert(self:getSourceRevision(instance) ~= nil, "Source observation belongs to another session.")
	return self.sourceSetter(instance, value, completed, serverOperation)
end

function InstanceMap:__assignStructuralProperty(instance, propertyName, value)
	instance[propertyName] = value
end

-- Native signals may still be queued when reconciliation reaches this instance. Capture
-- its actual values before deciding which pending generation a publication may replace.
function InstanceMap:captureStructuralChanges(instance)
	local id = self.fromInstances[instance]
	if id == nil then
		return
	end
	local signals = self.instancesToSignal[instance]
	self:__maybeFireInstanceChanged(instance, "Name")
	if
		self.fromInstances[instance] == id
		and self.fromIds[id] == instance
		and self.instancesToSignal[instance] == signals
	then
		self:__maybeFireInstanceChanged(instance, "Parent")
	end
end

-- Record only this remote assignment, not an instance-wide quiet period. Pre-recording
-- its value handles immediate signals; deferred signals still report any newer local value.
function InstanceMap:applyStructuralProperty(instance, propertyName, value)
	assert(propertyName == "Name" or propertyName == "Parent", "Expected a structural property.")
	if self.onBeforeStructuralAssignment ~= nil then
		self.onBeforeStructuralAssignment(instance, propertyName, value)
	end
	local id = self.fromInstances[instance]
	assert(id ~= nil, "Structural instance must be tracked.")
	local observed = if propertyName == "Parent" then self.observedParents else self.observedNames
	local previous = observed[instance]
	local assignments = self.structuralAssignments[instance] or {}
	self.structuralAssignments[instance] = assignments
	local predecessor = assignments[propertyName]
	local assignment = {}
	assignments[propertyName] = assignment
	observed[instance] = value

	local success, err = pcall(self.__assignStructuralProperty, self, instance, propertyName, value)
	-- A synchronous listener can remove/reinsert the instance or apply a nested assignment.
	-- Neither an old success nor its rollback may replace that newer observation state.
	if self.structuralAssignments[instance] == assignments and assignments[propertyName] == assignment then
		assignments[propertyName] = if success then nil else predecessor
		if next(assignments) == nil then
			self.structuralAssignments[instance] = nil
		end
		if self.fromInstances[instance] == id and self.fromIds[id] == instance then
			if success then
				if propertyName == "Parent" then
					self.knownParents[instance] = value
				end
			elseif observed[instance] == value then
				observed[instance] = previous
			end
		end
	end
	return success, err
end

function InstanceMap:__fmtDebug(output)
	output:writeLine("InstanceMap {{")
	output:indent()

	-- Collect all of the entries in the InstanceMap and sort them by their
	-- label, which helps make our output deterministic.
	local entries = {}
	for id, instance in self.fromIds do
		local label = string.format("%s (%s)", instance:GetFullName(), instance.ClassName)

		table.insert(entries, { id, label })
	end

	table.sort(entries, function(a, b)
		return a[2] < b[2]
	end)

	for _, entry in entries do
		output:writeLine("{}: {}", entry[1], entry[2])
	end

	output:unindent()
	output:write("}")
end

function InstanceMap:insert(id, instance)
	self:removeId(id)
	self:removeInstance(instance)

	self.fromIds[id] = instance
	self.fromInstances[instance] = id
	self.knownParents[instance] = instance.Parent
	self.observedNames[instance] = instance.Name
	self.observedParents[instance] = instance.Parent
	self:__connectSignals(instance)
end

function InstanceMap:removeId(id)
	local instance = self.fromIds[id]

	if instance ~= nil then
		self:__disconnectSignals(instance)
		self.fromIds[id] = nil
		self.fromInstances[instance] = nil
	end
end

function InstanceMap:removeInstance(instance, reason)
	local id = self.fromInstances[instance]
	self:__disconnectSignals(instance, reason)

	if id ~= nil then
		self.fromInstances[instance] = nil
		self.fromIds[id] = nil
	end
end

function InstanceMap:destroyInstance(instance)
	local id = self.fromInstances[instance]
	local descendants = instance:GetDescendants()

	-- Because the user might want to Undo this change, we cannot use Destroy
	-- since that locks that parent and prevents ChangeHistoryService from
	-- ever bringing it back. Instead, we parent to nil.
	if id ~= nil then
		local success, err = self:applyStructuralProperty(instance, "Parent", nil)
		if not success then
			error(err, 0)
		end
	else
		instance.Parent = nil
	end

	-- After the instance is successfully destroyed,
	-- we can remove all the id mappings

	if id ~= nil then
		self:removeId(id)
	end

	for _, descendantInstance in descendants do
		self:removeInstance(descendantInstance)
	end
end

function InstanceMap:destroyId(id)
	local instance = self.fromIds[id]
	if instance ~= nil then
		self:destroyInstance(instance)
	else
		-- There is no instance with this id, so we can just remove the id
		-- without worrying about instance destruction
		self:removeId(id)
	end
end

--[[
	Pause updates for an instance.
]]
function InstanceMap:pauseInstance(instance)
	local id = self.fromInstances[instance]

	-- If we don't know about this instance, ignore it.
	if id == nil then
		return
	end

	self.pausedUpdateInstances[instance] = true
end

--[[
	Unpause updates for an instance.
]]
function InstanceMap:unpauseInstance(instance)
	self.pausedUpdateInstances[instance] = nil
end

--[[
	Unpause updates for all instances.
]]
function InstanceMap:unpauseAllInstances()
	table.clear(self.pausedUpdateInstances)
end

function InstanceMap:__connectSignals(instance)
	if instance:IsA("LuaSourceContainer") then
		local sourceConnection = self.sourceSetter.Observe(instance, function(observation)
			self:__maybeFireInstanceChanged(instance, "Source", observation)
		end, function(outcome)
			if self.onSourceOutcome then
				self.onSourceOutcome(instance, outcome)
			elseif outcome.Status == "failed" then
				Log.warn("Source update failed for {}. {}", instance:GetFullName(), outcome.Error)
			end
		end, function(operation)
			if self.onSourceScheduled then
				self.onSourceScheduled(instance, operation)
			end
		end)
		local _, generation = self.sourceSetter.GetRevision(instance)
		if self.fromInstances[instance] == nil or generation ~= sourceConnection.Generation then
			-- Adopting an existing operation can synchronously stop or replace this session.
			sourceConnection:Disconnect("stopped")
			return
		end
		self.sourceConnections[instance] = sourceConnection
	end

	-- ValueBase instances have an overriden version of the Changed signal that
	-- only detects changes to their Value property.
	--
	-- We can instead connect listener to each individual property that we care
	-- about on those objects (Name and Value) to emulate the same idea.
	if instance:IsA("ValueBase") then
		local signals = {
			instance:GetPropertyChangedSignal("Name"):Connect(function()
				self:__maybeFireInstanceChanged(instance, "Name")
			end),

			instance:GetPropertyChangedSignal("Value"):Connect(function()
				self:__maybeFireInstanceChanged(instance, "Value")
			end),

			instance:GetPropertyChangedSignal("Parent"):Connect(function()
				self:__maybeFireInstanceChanged(instance, "Parent")
			end),

			instance.AttributeChanged:Connect(function(attributeName)
				if not UnmanagedAttributes.isUnmanaged(attributeName) then
					self:__maybeFireInstanceChanged(instance, "Attributes")
				end
			end),
		}

		self.instancesToSignal[instance] = signals
	else
		self.instancesToSignal[instance] = {
			instance.Changed:Connect(function(propertyName)
				self:__maybeFireInstanceChanged(instance, propertyName)
			end),
			instance.AttributeChanged:Connect(function(attributeName)
				if not UnmanagedAttributes.isUnmanaged(attributeName) then
					self:__maybeFireInstanceChanged(instance, "Attributes")
				end
			end),
		}
	end

	table.insert(
		self.instancesToSignal[instance],
		instance.ChildAdded:Connect(function(child)
			self:__maybeFireChildAdded(instance, child)
		end)
	)

	-- AncestryChanged is deliberately not watched. Changes to an instance's own Parent
	-- already arrive through the signals above, while AncestryChanged also fires on every
	-- descendant when an ancestor moves. Those descendants still have their parent, so
	-- reporting them made two-way sync warn about every instance inside a moved or
	-- deleted folder, and about every nested instance created by the initial sync.
end

function InstanceMap:__maybeFireInstanceChanged(instance, propertyName, observation)
	if self.fromInstances[instance] == nil then
		return
	end
	-- Source has one revision boundary for both the editor buffer and closed scripts.
	-- Instance.Changed is a duplicate and must never hide an open editor draft.
	if propertyName == "Source" and observation == nil then
		return
	end
	Log.trace("{}.{} changed", instance:GetFullName(), propertyName)

	local structural = propertyName == "Name" or propertyName == "Parent"
	if structural then
		local observed = if propertyName == "Parent" then self.observedParents else self.observedNames
		local value = instance[propertyName]
		if observed[instance] == value then
			return
		end
		observed[instance] = value
	end

	if self.pausedUpdateInstances[instance] and propertyName ~= "Source" and not structural then
		return
	end

	if self.onInstanceChanged == nil then
		return
	end

	if self:__isGameRunning() then
		-- We probably don't want to pick up property changes to save to the
		-- filesystem in a running game.
		return
	end

	self.onInstanceChanged(instance, propertyName, observation)
end

--[[
	Reports a child that isn't known yet. Instances Rojo+ creates itself also
	pass through here, between being parented and being inserted, so whoever
	handles this must check again later whether the child is still unknown.
]]
function InstanceMap:__maybeFireChildAdded(parent, child)
	if self.onChildAdded == nil or self.fromInstances[child] ~= nil then
		return
	end

	if self.pausedUpdateInstances[parent] or self:__isGameRunning() then
		return
	end

	self.onChildAdded(child)
end

-- Kept separate so tests, which the test place runs in a running game, can still exercise
-- change reporting.
function InstanceMap:__isGameRunning()
	return RunService:IsRunning()
end

function InstanceMap:__disconnectSignals(instance, reason)
	local sourceConnection = self.sourceConnections[instance]
	self.sourceConnections[instance] = nil
	if sourceConnection then
		sourceConnection:Disconnect(reason or "untracked")
	end
	local signals = self.instancesToSignal[instance]
	self.knownParents[instance] = nil
	self.observedNames[instance] = nil
	self.observedParents[instance] = nil
	self.structuralAssignments[instance] = nil
	self.pausedUpdateInstances[instance] = nil

	if signals ~= nil then
		-- In most cases, we only have a single signal, so we avoid keeping
		-- around the extra table. ValueBase objects force us to use multiple
		-- signals to emulate the Instance.Changed event, however.
		if typeof(signals) == "table" then
			for _, signal in signals do
				signal:Disconnect()
			end
		else
			signals:Disconnect()
		end

		self.instancesToSignal[instance] = nil
	end
end

return InstanceMap
