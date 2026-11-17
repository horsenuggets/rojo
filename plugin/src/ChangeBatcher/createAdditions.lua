--[[
	Turns instances created in Studio into additions for a write request.

	Each candidate that is still unknown and sits directly under a known parent
	becomes one addition: its subtree serialized as a model, and the ids picked
	for every instance in it. Candidates that were removed again, or moved under
	another new instance, are skipped, since that other instance carries them.

	Descendants that are already known stay out of the model. They were moved
	into the new subtree, so they are sent as moves instead, which keeps their
	ids and files.

	The new instances are inserted into the instance map before returning, so
	changes in the same batch can refer to them.
]]

local HttpService = game:GetService("HttpService")

local Packages = script.Parent.Parent.Parent.Packages
local Log = require(Packages.Log)

local function newId()
	return string.lower((string.gsub(HttpService:GenerateGUID(false), "-", "")))
end

-- The children a model of `instance` would hold. Instances that aren't
-- archivable are never serialized.
local function archivableChildren(instance)
	local children = {}
	for _, child in instance:GetChildren() do
		if child.Archivable then
			table.insert(children, child)
		end
	end
	return children
end

return function(instanceMap, candidates, serialize, makeId)
	makeId = makeId or newId
	local additions = {}
	local inserts = {}

	local function describe(instance)
		local entry = {
			id = makeId(),
			name = instance.Name,
			className = instance.ClassName,
			children = {},
		}
		table.insert(inserts, { entry.id, instance })
		for _, child in archivableChildren(instance) do
			if instanceMap.fromInstances[child] == nil then
				table.insert(entry.children, describe(child))
			end
		end
		return entry
	end

	-- Removes the copies of known descendants from a clone of `original`.
	-- Clones keep the order of archivable children, which is checked as well.
	local function prune(original, copy)
		local originals = archivableChildren(original)
		local copies = copy:GetChildren()
		assert(#originals == #copies, "The copy's children don't match the original")
		for index, child in originals do
			local copied = copies[index]
			assert(
				copied.Name == child.Name and copied.ClassName == child.ClassName,
				"The copy's children don't match the original"
			)
			if instanceMap.fromInstances[child] ~= nil then
				copied:Destroy()
			else
				prune(child, copied)
			end
		end
	end

	for instance in candidates do
		local parent = instance.Parent
		local parentId = parent and instanceMap.fromInstances[parent]
		if instanceMap.fromInstances[instance] ~= nil or parentId == nil or not instance.Archivable then
			continue
		end

		-- The copy and the ids are taken together, before serializing yields, so
		-- they describe the same subtree. Tracking starts right away, so edits
		-- made while serializing are queued like any other.
		local copied, copy = pcall(function()
			local clone = instance:Clone()
			prune(instance, clone)
			return clone
		end)
		if not copied then
			Log.warn("Could not send {} to the file system. {}", instance:GetFullName(), copy)
			continue
		end
		local first = #inserts + 1
		local ids = describe(instance)
		for index = first, #inserts do
			local entry = inserts[index]
			instanceMap:insert(entry[1], entry[2])
		end

		local success, result = pcall(serialize, copy)
		copy:Destroy()
		if not success then
			Log.warn("Could not send {} to the file system. {}", instance:GetFullName(), result)
			for index = first, #inserts do
				instanceMap:removeId(inserts[index][1])
			end
			continue
		end

		additions[ids.id] = {
			parent = parentId,
			model = result,
			ids = ids,
		}
	end

	table.clear(candidates)
	return additions
end
