--[[
	Describes a patch the way the sync confirmation view shows it, so automated
	checks can assert that a preview contains no unexpected removals or changes.
	Each list is capped at `limit` entries; the counts are always complete.
]]

local function sortedKeys(map)
	local keys = {}
	for key in map do
		table.insert(keys, key)
	end
	table.sort(keys)
	return keys
end

local function addedPath(patch, instanceMap, id)
	local virtualInstance = patch.added[id]
	local parentId = virtualInstance.Parent
	local parentInstance = instanceMap.fromIds[parentId]
	if parentInstance ~= nil then
		return parentInstance:GetFullName() .. "." .. virtualInstance.Name
	elseif patch.added[parentId] ~= nil then
		return addedPath(patch, instanceMap, parentId) .. "." .. virtualInstance.Name
	end
	return virtualInstance.Name
end

return function(patch, instanceMap, limit)
	local summary = {
		removed = {},
		added = {},
		updated = {},
		counts = { removed = #patch.removed, added = 0, addedRoots = 0, updated = #patch.updated },
	}

	for _, item in patch.removed do
		if #summary.removed >= limit then
			break
		end
		local instance = if typeof(item) == "Instance" then item else instanceMap.fromIds[item]
		table.insert(summary.removed, if instance then instance:GetFullName() else tostring(item))
	end

	-- Only the roots of added subtrees are listed, since descendants are implied.
	for id, virtualInstance in patch.added do
		summary.counts.added += 1
		if patch.added[virtualInstance.Parent] ~= nil then
			continue
		end
		summary.counts.addedRoots += 1
		if #summary.added < limit then
			table.insert(summary.added, addedPath(patch, instanceMap, id))
		end
	end
	table.sort(summary.added)

	for _, update in patch.updated do
		if #summary.updated >= limit then
			break
		end
		local instance = instanceMap.fromIds[update.id]
		table.insert(summary.updated, {
			path = if instance then instance:GetFullName() else tostring(update.id),
			properties = sortedKeys(update.changedProperties or {}),
			changedName = update.changedName,
			changedClassName = update.changedClassName,
		})
	end

	return summary
end
