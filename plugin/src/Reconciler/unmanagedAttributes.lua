--[[
	Some attributes are never written to the filesystem, so their absence there
	says nothing about whether they should exist in Studio:

	> Internal attributes, prefixed with `RBX`, which syncback skips.
	> Rojo's own reference links, `Rojo_Id` and `Rojo_Target_*`, which the server
	  resolves into Ref properties instead of sending.
	> Rojo+'s own connection markers, prefixed with `__RojoPlus_`, which a build
	  bakes into a place and the filesystem never holds, so the first sync must
	  leave them in place instead of deleting them.

	These are left exactly as they are in Studio when diffing and applying.
	Values with NaN or infinity, alone or inside a composite like a Vector3, are
	written in a tagged form, so they're managed like any other value.
]]

local unmanagedAttributes = {}

function unmanagedAttributes.isUnmanaged(name)
	return string.sub(name, 1, 3) == "RBX"
		or name == "Rojo_Id"
		or string.sub(name, 1, 12) == "Rojo_Target_"
		or string.sub(name, 1, 11) == "__RojoPlus_"
end

-- Returns the attributes to give an instance: the managed values from
-- `incoming`, plus the unmanaged ones the instance already has.
function unmanagedAttributes.merge(incoming, existing)
	local merged = {}
	for name, value in incoming do
		if not unmanagedAttributes.isUnmanaged(name) then
			merged[name] = value
		end
	end
	for name, value in existing or {} do
		if unmanagedAttributes.isUnmanaged(name) then
			merged[name] = value
		end
	end
	return merged
end

return unmanagedAttributes
