return function()
	const encodeCollisionGroups = require("./encodeCollisionGroups")

	it("encodes empty and default group payloads exactly", function()
		expect(encodeCollisionGroups({})).to.equal("\1\0")
		expect(encodeCollisionGroups({ { Id = 0, Mask = -1, Name = "Default" } })).to.equal(
			string.char(1, 1, 0, 4, 255, 255, 255, 255, 7) .. "Default"
		)
	end)

	it("keeps sparse actual IDs and masks without mutating API order", function()
		const groups = {
			{ Id = 7, Mask = 4294967295, Name = "Later" },
			{ Id = 2, Mask = -4, Name = "Earlier" },
		}
		const first = encodeCollisionGroups(groups)
		expect(first).to.equal(encodeCollisionGroups(groups))
		expect(groups[1].Id).to.equal(7)
		expect(groups[2].Id).to.equal(2)
		const encoded = buffer.fromstring(first)
		expect(buffer.readu8(encoded, 1)).to.equal(2)
		expect(buffer.readu8(encoded, 2)).to.equal(2)
		expect(buffer.readi32(encoded, 4)).to.equal(-4)
		expect(buffer.readu8(encoded, 16)).to.equal(7)
		expect(buffer.readu32(encoded, 18)).to.equal(4294967295)
	end)

	it("stores UTF-8 names by byte length", function()
		const encoded = buffer.fromstring(encodeCollisionGroups({ { Id = 0, Mask = 0, Name = "Café" } }))
		expect(buffer.readu8(encoded, 8)).to.equal(#"Café")
		expect(buffer.readstring(encoded, 9, #"Café")).to.equal("Café")
	end)

	it("accepts every ID and preserves every mask bit", function()
		const groups = {}
		for id = 0, 31 do
			table.insert(groups, { Id = id, Mask = 2 ^ id, Name = `Group{id}` })
		end
		const encoded = buffer.fromstring(encodeCollisionGroups(groups))
		expect(buffer.readu8(encoded, 1)).to.equal(32)
		local offset = 2
		for id = 0, 31 do
			expect(buffer.readu8(encoded, offset)).to.equal(id)
			expect(buffer.readu32(encoded, offset + 2)).to.equal(2 ^ id)
			offset += 7 + #`Group{id}`
		end
		expect(offset).to.equal(buffer.len(encoded))
	end)

	it("rejects ambiguous IDs, duplicate names and unrepresentable fields", function()
		const invalid = {
			{ { Id = 0, Mask = 0, Name = "A" }, { Id = 0, Mask = 1, Name = "B" } },
			{ { Id = 0, Mask = 0, Name = "A" }, { Id = 1, Mask = 1, Name = "A" } },
			{ { Id = 32, Mask = 0, Name = "A" } },
			{ { Id = -1, Mask = 0, Name = "A" } },
			{ { Id = 0.5, Mask = 0, Name = "A" } },
			{ { Id = 0, Mask = math.huge, Name = "A" } },
			{ { Id = 0, Mask = -2147483649, Name = "A" } },
			{ { Id = 0, Mask = 4294967296, Name = "A" } },
			{ { Id = 0, Mask = 0.5, Name = "A" } },
			{ { Id = 0, Mask = 0, Name = string.rep("A", 256) } },
		}
		for _, groups in invalid do
			expect(pcall(encodeCollisionGroups, groups)).to.equal(false)
		end
	end)
end
