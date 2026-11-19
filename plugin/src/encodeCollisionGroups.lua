--[[

encodeCollisionGroups

Encodes the version-one CollisionGroupData payload from public PhysicsService results.
Uses actual group IDs, since registration gaps and API iteration order are not identities.

--]]

return function(groups)
	assert(type(groups) == "table", "Collision groups must be an array.")
	assert(#groups <= 32, "Version-one collision data supports at most 32 groups.")
	const sorted = table.clone(groups)
	const identifiers = {}
	const names = {}
	local size = 2
	for _, group in sorted do
		assert(type(group) == "table", "Collision group entries must be tables.")
		assert(
			type(group.Id) == "number" and group.Id % 1 == 0 and group.Id >= 0 and group.Id < 32,
			"Collision group IDs must be integers from zero through 31."
		)
		assert(
			type(group.Name) == "string" and #group.Name <= 255,
			"Collision group names must fit their byte-length field."
		)
		assert(
			type(group.Mask) == "number"
				and group.Mask % 1 == 0
				and group.Mask >= -2147483648
				and group.Mask <= 4294967295,
			"Collision group masks must fit 32 bits."
		)
		assert(not identifiers[group.Id] and not names[group.Name], "Collision group IDs and names must be unique.")
		identifiers[group.Id] = true
		names[group.Name] = true
		size += 7 + #group.Name
	end
	table.sort(sorted, function(a, b)
		return a.Id < b.Id
	end)
	const encoded = buffer.create(size)
	buffer.writeu8(encoded, 0, 1)
	buffer.writeu8(encoded, 1, #sorted)
	local offset = 2
	for _, group in sorted do
		buffer.writeu8(encoded, offset, group.Id)
		buffer.writeu8(encoded, offset + 1, 4)
		buffer.writeu32(encoded, offset + 2, group.Mask % 4294967296)
		buffer.writeu8(encoded, offset + 6, #group.Name)
		buffer.writestring(encoded, offset + 7, group.Name)
		offset += 7 + #group.Name
	end
	return buffer.tostring(encoded)
end
