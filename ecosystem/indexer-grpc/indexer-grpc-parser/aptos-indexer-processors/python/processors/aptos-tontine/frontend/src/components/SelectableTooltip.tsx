import {
  Popover,
  PopoverTrigger,
  PopoverContent,
  PopoverBody,
  Portal,
  Button,
} from "@chakra-ui/react";
import { ReactNode } from "react";
import { CopyIcon } from "@chakra-ui/icons";

export function SelectableTooltip({
  textComponent,
  label,
}: {
  textComponent: ReactNode;
  label: string;
}) {
  // https://stackoverflow.com/questions/76252010/how-to-change-content-direction-of-popover
  return (
    <Popover trigger="hover" openDelay={75} closeDelay={100}>
      <PopoverTrigger>{textComponent}</PopoverTrigger>
      <Portal>
        <PopoverContent>
          <PopoverBody>{label}</PopoverBody>
          <Button
            rightIcon={<CopyIcon />}
            size="sm"
            onClick={() => navigator.clipboard.writeText(label)}
          />
        </PopoverContent>
      </Portal>
    </Popover>
  );
}
