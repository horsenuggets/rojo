return function()
	local trueEquals = require(script.Parent.trueEquals)

	local nan = 0 / 0
	local specials = { 0, 1, math.huge, -math.huge, nan }

	-- Builds each composite type with `value` in one of its number slots.
	local function composites(value)
		return {
			Vector2.new(value, 2),
			Vector3.new(1, value, 3),
			CFrame.new(1, 2, value),
			NumberRange.new(value),
			UDim.new(value, 4),
			UDim2.new(0.5, 1, value, 2),
			Rect.new(1, 2, 3, value),
			Ray.new(Vector3.new(value, 0, 0), Vector3.new(0, 1, 0)),
			NumberSequence.new({
				NumberSequenceKeypoint.new(0, value),
				NumberSequenceKeypoint.new(1, 1),
			}),
		}
	end

	-- Every pair of special values, alone and inside every composite type:
	-- each value must only equal itself, NaN included.
	it("should only treat special values as equal to themselves", function()
		for i, a in specials do
			for j, b in specials do
				expect(trueEquals(a, b)).to.equal(i == j)
				local aComposites, bComposites = composites(a), composites(b)
				for index, aComposite in aComposites do
					local bComposite = bComposites[index]
					expect(trueEquals(aComposite, bComposite)).to.equal(i == j)
				end
			end
		end
	end)

	it("should not treat different composites with NaN components as equal", function()
		expect(trueEquals(Vector3.new(nan, 0, 0), Vector3.new(nan, 5, 5))).to.equal(false)
		expect(trueEquals(Vector3.new(nan, 0, 0), nan)).to.equal(false)
	end)

	-- selene: allow(roblox_incorrect_color3_new_bounds)
	it("should compare colors with NaN channels", function()
		expect(trueEquals(Color3.new(nan, 0, 0), Color3.new(nan, 0, 0))).to.equal(true)
		expect(trueEquals(Color3.new(nan, 0, 0), Color3.new(0, 0, 0))).to.equal(false)
	end)

	it("should still allow small floating point differences", function()
		expect(trueEquals(Vector3.new(1, 2, 3), Vector3.new(1.00001, 2, 3))).to.equal(true)
		expect(trueEquals(NumberRange.new(0, 1), NumberRange.new(0, 1.00001))).to.equal(true)
		expect(trueEquals(Vector3.new(1, 2, 3), Vector3.new(1.1, 2, 3))).to.equal(false)
	end)
end
