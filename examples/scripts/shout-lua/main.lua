function on_event(event)
  if event.type ~= 'command' or event.data.command ~= 'shout' then return {} end
  local command = event.data
  for _, option in ipairs(command.options) do
    if option.name == 'text' and option.value.type == 'string' then
      return {{type='respond', interaction_id=command.interaction_id, token=command.token,
        response={kind='message', reply={content=string.upper(option.value.value),
          embeds=closure.array({}), ephemeral=false}}}}
    end
  end
  return {}
end
