--[[
	Determines the order in which `ServeSession:__replaceInstances` should swap
	instances so that sibling order is preserved.

	Roblox appends to `GetChildren()` on every reparent, so the order in which we
	re-parent replacements determines their final sibling order. To rebuild
	`GetChildren()` exactly as it was before the swap we must:

	* process ancestors before descendants, so each replacement's parent already
	  exists when we re-parent the replacement, and
	* process siblings in their original `GetChildren()` order.

	`swaps` is an array of `{ id, replacement, oldInstance }` entries. This sorts
	the array in place (annotating each entry with `depth`/`siblingIndex`) and
	returns it.
]]
local function orderSwaps(swaps)
	-- Cache each parent's child-to-index map so finding every sibling's
	-- position stays linear in the parent's child count instead of scanning
	-- the full child array once per swap.
	local indexByParent = {}

	local function findSiblingIndex(parent, child)
		local indices = indexByParent[parent]
		if indices == nil then
			indices = {}
			for index, sibling in parent:GetChildren() do
				indices[sibling] = index
			end
			indexByParent[parent] = indices
		end
		return indices[child] or 0
	end

	for _, swap in swaps do
		local depth = 0
		local ancestor = swap.oldInstance.Parent
		while ancestor ~= nil do
			depth += 1
			ancestor = ancestor.Parent
		end
		swap.depth = depth

		local siblingIndex = 0
		local parent = swap.oldInstance.Parent
		if parent ~= nil then
			siblingIndex = findSiblingIndex(parent, swap.oldInstance)
		end
		swap.siblingIndex = siblingIndex
	end

	table.sort(swaps, function(a, b)
		if a.depth ~= b.depth then
			return a.depth < b.depth
		end
		return a.siblingIndex < b.siblingIndex
	end)

	return swaps
end

return orderSwaps
