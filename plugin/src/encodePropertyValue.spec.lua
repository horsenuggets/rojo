return function()
	const encodePropertyValue = require("./encodePropertyValue")
	const encodeProperty = require("./ChangeBatcher/encodeProperty")

	it("keeps backup attributes separate from syncback ownership filtering", function()
		for _, authored in { false, true } do
			for _, engine in { false, true } do
				const attributes = {}
				if authored then
					attributes.Authored = "preserved"
				end
				if engine then
					attributes.RBX_OriginalTechnologyOnFileLoad = 3
				end
				const originalCount = (if authored then 1 else 0) + (if engine then 1 else 0)
				const backupOk, backup = encodePropertyValue(attributes, "Attributes")
				const syncOk, sync = encodeProperty({}, "Attributes", {
					dataType = "Attributes",
					read = function()
						return true, attributes
					end,
				}, nil)
				expect(backupOk).to.equal(true)
				expect(syncOk).to.equal(true)
				expect(backup.Attributes.Authored ~= nil).to.equal(authored)
				expect(backup.Attributes.RBX_OriginalTechnologyOnFileLoad ~= nil).to.equal(engine)
				expect(sync.Attributes.Authored ~= nil).to.equal(authored)
				expect(sync.Attributes.RBX_OriginalTechnologyOnFileLoad).to.equal(nil)
				local count = 0
				for _ in attributes do
					count += 1
				end
				expect(count).to.equal(originalCount)
				const repeatedOk, repeated = encodePropertyValue(attributes, "Attributes")
				expect(repeatedOk).to.equal(true)
				expect(repeated.Attributes.Authored ~= nil).to.equal(authored)
				expect(repeated.Attributes.RBX_OriginalTechnologyOnFileLoad ~= nil).to.equal(engine)
			end
		end
	end)

	it("encodes empty security capabilities as an exact unsigned integer", function()
		const Http = require(script.Parent.Parent.Packages.Http)
		const ok, encoded = encodePropertyValue(SecurityCapabilities.new(), "SecurityCapabilities")
		expect(ok).to.equal(true)
		expect(encoded.SecurityCapabilities.mostSignificantPart).to.equal(0)
		expect(encoded.SecurityCapabilities.leastSignificantPart).to.equal(0)
		expect(Http.msgpackEncode(encoded.SecurityCapabilities)).to.equal(string.char(207, 0, 0, 0, 0, 0, 0, 0, 0))
	end)

	it("encodes canonical capability bits without duplicating deprecated aliases", function()
		const Http = require(script.Parent.Parent.Packages.Http)
		for _, item in Enum.SecurityCapability:GetEnumItems() do
			const value = SecurityCapabilities.new(item)
			const canonical = Enum.SecurityCapability[tostring(value)]
			const ok, encoded = encodePropertyValue(value, "SecurityCapabilities")
			expect(ok).to.equal(true)
			const words = encoded.SecurityCapabilities
			expect(words.mostSignificantPart).to.equal(if canonical.Value >= 32 then 2 ^ (canonical.Value - 32) else 0)
			expect(words.leastSignificantPart).to.equal(if canonical.Value < 32 then 2 ^ canonical.Value else 0)
			const repeatedOk, repeated = encodePropertyValue(value, "SecurityCapabilities")
			expect(repeatedOk).to.equal(true)
			expect(Http.msgpackEncode(words)).to.equal(Http.msgpackEncode(repeated.SecurityCapabilities))
			expect(value).to.equal(SecurityCapabilities.new(item))
		end
	end)

	it("keeps high and low capability words exact on the MessagePack wire", function()
		const Http = require(script.Parent.Parent.Packages.Http)
		const value =
			SecurityCapabilities.new(Enum.SecurityCapability.RunClientScript, Enum.SecurityCapability.LoadUnownedAsset)
		const ok, encoded = encodePropertyValue(value, "SecurityCapabilities")
		expect(ok).to.equal(true)
		const lowBit = Enum.SecurityCapability.RunClientScript.Value
		expect(lowBit < 32).to.equal(true)
		const highBit = Enum.SecurityCapability.LoadUnownedAsset.Value
		expect(highBit >= 32).to.equal(true)
		const expected = buffer.create(9)
		buffer.writeu8(expected, 0, 207)
		for byte = 0, 3 do
			buffer.writeu8(expected, byte + 1, bit32.extract(2 ^ (highBit - 32), (3 - byte) * 8, 8))
			buffer.writeu8(expected, byte + 5, bit32.extract(2 ^ lowBit, (3 - byte) * 8, 8))
		end
		expect(Http.msgpackEncode(encoded.SecurityCapabilities)).to.equal(buffer.tostring(expected))
	end)

	it("rejects values that are not security capabilities", function()
		for _, value in { false, 0, "RunClientScript", {} } do
			const ok = encodePropertyValue(value, "SecurityCapabilities")
			expect(ok).to.equal(false)
		end
	end)

	it("preserves exact numeric components including infinity", function()
		for _, value in { -math.huge, -1, 0, 1, math.huge } do
			for _, dataType in { "Float32", "Float64" } do
				const ok, encoded = encodePropertyValue(value, dataType)
				expect(ok).to.equal(true)
				expect(encoded[dataType]).to.equal(value)
			end
			const vector = Vector3.new(value, 0, 1)
			const ok, encoded = encodePropertyValue(vector, "Vector3")
			expect(ok).to.equal(true)
			expect(encoded.Vector3[1]).to.equal(vector.X)
			expect(encoded.Vector3[2]).to.equal(vector.Y)
			expect(encoded.Vector3[3]).to.equal(vector.Z)
		end
	end)

	it("preserves exact numeric attribute values without mutating them", function()
		const attributes = { Infinite = math.huge, Vector = Vector2.new(-math.huge, math.huge) }
		const ok, encoded = encodePropertyValue(attributes, "Attributes")
		expect(ok).to.equal(true)
		expect(encoded.Attributes.Infinite.Float64).to.equal(math.huge)
		expect(encoded.Attributes.Vector.Vector2[1]).to.equal(-math.huge)
		expect(encoded.Attributes.Vector.Vector2[2]).to.equal(math.huge)
		expect(attributes.Infinite).to.equal(math.huge)
		expect(attributes.Vector.X).to.equal(-math.huge)
	end)
end
