return function()
	local encodePatchUpdate = require(script.Parent.encodePatchUpdate)

	it("should never encode attributes the filesystem can't hold", function()
		local part = Instance.new("Part")
		part:SetAttribute("Health", 5)
		part:SetAttribute("Broken", Vector3.new(0 / 0, 0, 0))
		part:SetAttribute("Rojo_Id", "linked")
		local update = encodePatchUpdate(part, "PART", { Attributes = true })
		local attributes = update.changedProperties.Attributes.Attributes
		expect(attributes.Health ~= nil).to.equal(true)
		expect(attributes.Broken).to.equal(nil)
		expect(attributes.Rojo_Id).to.equal(nil)
		part:Destroy()
	end)

	it("should encode non-finite number attributes exactly", function()
		local part = Instance.new("Part")
		part:SetAttribute("Far", math.huge)
		part:SetAttribute("Near", -math.huge)
		part:SetAttribute("Broken", 0 / 0)
		local update = encodePatchUpdate(part, "PART", { Attributes = true })
		local attributes = update.changedProperties.Attributes.Attributes
		expect(attributes.Far.Float64).to.equal(math.huge)
		expect(attributes.Near.Float64).to.equal(-math.huge)
		expect(attributes.Broken.Float64 ~= attributes.Broken.Float64).to.equal(true)
		part:Destroy()
	end)

	it("should encode non-finite number properties exactly", function()
		local value = Instance.new("NumberValue")
		for _, number in { math.huge, -math.huge } do
			value.Value = number
			local update = encodePatchUpdate(value, "VALUE", { Value = true })
			expect(update.changedProperties.Value.Float64).to.equal(number)
		end
		value:Destroy()
	end)

	it("should return an update when there are property changes", function()
		local part = Instance.new("Part")
		local properties = {
			CFrame = true,
			Color = true,
		}
		local update = encodePatchUpdate(part, "PART", properties)

		expect(update.id).to.equal("PART")
		expect(update.changedProperties.CFrame).to.be.ok()
		expect(update.changedProperties.Color).to.be.ok()
	end)

	it("should return nil when there are no property changes", function()
		local part = Instance.new("Part")
		local properties = {
			NonExistentProperty = true,
		}
		local update = encodePatchUpdate(part, "PART", properties)

		expect(update).to.equal(nil)
	end)

	it("should set changedName in the update when the instance's Name changes", function()
		local part = Instance.new("Part")
		local properties = {
			Name = true,
		}

		part.Name = "We'reGettingToTheCoolPart"

		local update = encodePatchUpdate(part, "PART", properties)

		expect(update.changedName).to.equal("We'reGettingToTheCoolPart")
	end)

	it("should correctly encode property values", function()
		local part = Instance.new("Part")
		local properties = {
			Position = true,
			Size = true,
			Color = true,
		}

		part.Position = Vector3.new(0, 100, 0)
		part.Size = Vector3.new(2, 100, 4)
		part.Color = Color3.new(0.8, 0.2, 0.9)

		local update = encodePatchUpdate(part, "PART", properties)
		local size = update.changedProperties.Size
		local color = update.changedProperties.Color

		-- Position doesn't serialize; it's carried by CFrame.
		expect(update.changedProperties.Position).to.equal(nil)

		expect(size.Vector3[1]).to.equal(2)
		expect(size.Vector3[2]).to.equal(100)
		expect(size.Vector3[3]).to.equal(4)

		expect(color.Color3[1]).to.be.near(0.8, 0.01)
		expect(color.Color3[2]).to.be.near(0.2, 0.01)
		expect(color.Color3[3]).to.be.near(0.9, 0.01)
	end)
end
